# Paired GroundStation and RRLS checklists

Checklist definitions and progress use the existing station installation pairing.
Both stations must run a version supporting the signed `MS-v2` journal profile.
Unpaired stations retain their local checklist JSON behavior. HITL retains its
existing checklist handling and does not start the paired checklist worker.

The main GroundStation publishes its initial definition catalog. RRLS receives it
without publishing its defaults over the primary. A genuine offline check can
still be saved before first contact. An administrator explicitly editing RRLS's
template before first contact publishes the whole explicit template. Conflicting
offline edits remain visible for review; neither station silently wins.

## Storage and restart

Next to the configured `GS_CHECKLISTS_FILE`, each station creates a
`<station-id>.paired.sqlite3` journal and a small
`<station-id>.paired-selector.json` selector. Keep both, including SQLite WAL
files when copying a running installation. Stop the service before making a plain
filesystem backup. The original JSON remains available as migration input and
is not rewritten for every paired checkbox.

A checkbox writes one progress document and its persisted observed causal
context. Its materialized progress row is separate from the immutable definition
snapshot. Raw histories retain tombstones and concurrent heads. The host journal
allows up to 8,192 documents and 64 MiB encoded history; the active template is
limited to 32 lists, 1,024 total items and an actual serialized 1 MiB. Quota
failure preserves the previous visible template and reports an error.

A complete candidate must prove its policy/list/item membership before it can
replace the active template. Preparation and fsync happen outside operational
locks. The final safe-state check, selector rename and matching in-memory view
switch happen together; directory durability is completed outside those locks.
If durability is uncertain, non-emergency procedure starts remain blocked until
recovery finishes. Restart loads the selected complete generation and persistent
progress. View revisions increase across restart so connected clients can reseed.

## Procedures and conflicts

Definition changes and policy changes wait for Idle, explicitly cleared local
qualification, no running procedure, no pending zero and drained local checklist
receipts. Progress remains editable during Fire/recovery. Changed wording, test
binding or list generation invalidates old progress; reordering preserves its
semantic receipt binding. Conflicting progress is unchecked until reviewed.

The Checklists page shows synchronization readiness and a bounded set of
conflicts. Administrators select an existing reviewed version; resolution uses
an exact causal compare-and-set. A newer concurrent edit requires another review.
Collaborators can check items, but cannot edit definitions or resolve conflicts.
Automated sequence passes complete matching items and persist local receipt IDs;
a delayed replay cannot restore a later manually cleared item.

Shared completion is checklist data only. It never imports operational test
qualification, nitrogen overrides, abort state, quick-disconnect state or launch
commands. New local operational evidence is bound to the exact active template.
Existing unbound qualification requires an explicit local reset after pairing.
Wait for synchronization readiness before resetting qualification; a legacy
pending event which cannot match the active template is reported for explicit
reset/retest, never rebound to new definitions automatically.

## Transport and diagnostics

Legacy mission settings and the checklist journal share one 256-byte/second
signed-envelope allowance. There is one pending frame per profile, fair turns,
bounded inboxes/assemblies and exact-frame retries under backpressure. This is
background traffic below control/GPS, not a second command channel. The byte
allowance covers signed envelopes, not UART framing overhead.

`GET /api/checklists/sync` reports readiness, durability/definition pending,
qualification binding and the first eight indexed conflicts. It does not scan
or serialize all histories on every UI poll. Stable staged templates also do
not reload full histories on each worker tick.

The isolated lifecycle test uses fake databases and signed, lossy two-station
replication. It covers offline conflicts, restart, explicit secondary bootstrap,
receipt replay, per-item writes, staged definitions, failed materialization,
selector rename/fsync recovery and crash restoration. Firmware-bay network soaks
remain a separate release qualification gate.
