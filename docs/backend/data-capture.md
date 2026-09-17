# Recorded telemetry CSV downloads

CSV downloads are available in normal (main), HITL, and Test Fire builds.

1. In **Actions**, start recording before the test.
2. Stop recording after the test to include all flushed samples.
3. Open the **Data Export** tab, refresh the list, select the
   session, and click **Download CSV**.

Browser clients use the browser download location. Native clients save to
Downloads (Documents if Downloads is unavailable) and show the saved path.
An active recording can also be downloaded: the export ends at the last committed
row when requested, not at the eventual end of that recording.

The export contains every recorded telemetry row, without chart decimation,
resampling, or additional calibration. It is long-form CSV: one message per row,
with these columns:

| Column | Contents |
| --- | --- |
| id | Recording-local row ID |
| received_timestamp_ms | Timestamp stored by the receiver |
| source_timestamp_ms | Original source timestamp, blank if unavailable |
| sender_id | Recorded source identity |
| data_type | Telemetry type |
| values_json | Original decoded values as a JSON array, when present |
| payload_json | Original payload byte array, when present |

JSON cells are quoted CSV fields. Spreadsheet-formula prefixes in text cells
are escaped. Values remain those received from firmware: this is not an SD-card
download or a substitute for samples that were never transmitted or recorded.

## Backend API

- GET /api/recordings: array of {id, bytes, active} for recording databases.
- GET /api/recordings/{id}/csv: streamed CSV attachment.
- GET /api/recordings/csv?start_ms=...&end_ms=...: automatically search all local
  recordings by receive timestamp (UTC milliseconds, inclusive start/exclusive end).

The Data Export tab includes UTC date/time inputs for the range export. See
[System date/time](system-time.md) for the separate per-user `set_system_time`
permission, client/network clock synchronization, and required Linux host setup.

All recording exports require the existing **ViewData** permission; clients send their usual
Bearer token when authenticated. Recording start/stop and all command permissions
are unchanged. Only session filenames in the configured recording directory are
accepted; arbitrary paths, symlinks, and non-recording databases are rejected.
Downloads open SQLite read-only and read in 512-row pages, so they do not load an
entire session into backend memory. Browser clients buffer their download as a
Blob; use the native client or an authenticated streaming HTTP client for very
large captures. The bytes field describes the SQLite file, not the eventual CSV size.

The pre-existing Test Fire-specific automatic CSV exporter is unchanged.
**Start/Pause/Cancel Fill, Nitrogen test, and Valve self-test** remain in Actions,
including with older/custom layouts. Their permissions, arming requirements,
confirmation dialogs, and safety interlocks still apply.
