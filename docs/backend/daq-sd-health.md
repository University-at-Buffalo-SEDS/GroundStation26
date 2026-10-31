# DAQ SD health and manual retry

`BOARD_HEALTH` remains version 1 with exactly 24 UInt32 words on every board.
DAQ additionally sends `DAQ_SD_HEALTH` (canonical ID `92486907`), eight UInt32
words every five seconds, to `GROUND_STATION`:

| Word | Meaning |
| --- | --- |
| 0 | Flags: bit 0 services initialized, bit 1 card ready, bit 2 launch logging finished, bit 3 retry pending/active |
| 1 | Age of last successful sector write in milliseconds; UInt32 MAX means never |
| 2 | Age of last successful media flush in milliseconds; UInt32 MAX means never |
| 3 | SD initialization stage |
| 4 | Initialization/retry attempt counter |
| 5 | Last accepted manual-retry token |
| 6 | Last completed manual-retry token |
| 7 | Initialization failure counter |

Only canonical DAQ senders can supply this companion. Invalid lengths or
reserved flag bits are rejected. Companion receipt freshness is tracked
separately from base diagnostics and ordinary traffic. Base reports cannot make
old SD status fresh. Existing non-DAQ/older-board diagnostics remain unchanged.

The `/api/network_topology` health entries and NetworkTopology WebSocket snapshots
expose optional diagnostics fields `sd_flags`,
`sd_last_write_age_ms`, `sd_last_flush_age_ms`, `sd_init_stage`, `sd_retry_count`,
`sd_retry_requested_token`, `sd_retry_completed_token`, `sd_init_failures`, and
`sd_status_received_age_ms`. A never-written/flushed sentinel becomes a null age;
`sd_flags` being present distinguishes support from old firmware. Reported write
and flush ages are measured at DAQ report generation; add companion receipt age
when projecting their age between reports. A fresh ready flag describes card
readiness; it does not prove a recent successful write.

## Retry API

`POST /api/network-health/daq/sd/retry` accepts an empty request or `{}`. It has no
board-selection or client-token parameter. It requires a current signed-in user
with `send_commands` and command access to `DaqSdRetry`. RRLS is read-only; use
the primary GroundStation. Primary normal, test-fire and HITL builds use the same
storage-maintenance operation.

A successful local SDK admission returns HTTP 202:

```json
{"accepted":true,"token":123,"status":"queued","message":"Retry command queued; waiting for DAQ diagnostics. This does not confirm an SD write."}
```

The generated nonzero UInt32 token is sent directly to `SD_CARD` as
`DAQ_SD_RETRY` (canonical ID `24116261`), reliable unordered, priority 240. It is
not a managed network variable, is not persisted by GroundStation, and is not
replayed on connection or service restart. Firmware deduplicates retry tokens.
The operation requests reinitialization, never formatting.

HTTP 401/403 denotes authentication/permission or RRLS rejection, 409 an
outstanding retry, 429 the five-second request limit, and 503 unavailable/stale
DAQ status or failed SDK admission. Companion diagnostics must be no more than
12 seconds old and report initialized services. A pending/active DAQ flag or an
outstanding reliable transport request prevents another retry. A locally queued
request without confirmation can time out after 15 seconds only when transport
and DAQ do not report an outstanding attempt. DAQ may legitimately stay pending
for much longer while card initialization runs.

Accepted and completed tokens describe progress, not write success. Completion
means the reinitialization attempt finished, whether it succeeded or failed.
Card readiness and last-write/flush ages must still be checked independently.
