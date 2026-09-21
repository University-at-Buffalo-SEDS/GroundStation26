# Data Export: preview, select and report

The dedicated **Data Export** tab provides numeric-channel graphs, optional UTC
date bounds across recordings, per-channel selection and two time-window sliders.
The selected interval is highlighted on each preview. Choose **entire loaded time
span** and **select all channels** to export everything loaded, or narrow either.
Intervals include the start and exclude the end. File saves stay inside native
clients; no external browser tab is opened.

- **CSV:** one numeric channel sample per row, with recording/row identity,
  receive/source timestamps, value and regression/provenance JSON.
- **Excel:** full selected samples, editable scatter charts, and a regression
  metadata sheet (long metadata is split across cells without truncation).
- **PDF:** graphs, regression/provenance pages and a paginated table containing
  every selected sample. PDF graphs do not apply the preview's downsampling.

Previews are peak-preserving and bounded to 600 points/channel. Report downloads
contain the selected recorded values without UI smoothing or recalibration.
These reports handle numeric telemetry; the original message CSV described below
also retains nonnumeric packets and raw payload bytes.

New backend builds record the actual loadcell/pressure calibration configuration,
including the selected fill loadcell and absolute-value mapping when exporting
fill percentage. This does not modify signed raw or calibrated weight samples.
The recorded metadata also includes
fit coefficients, fit type, x-offset, tare, calibration points and fill targets in
each recording's `calibration_history` table. Changes form separate provenance
epochs in the export. Older recordings without that table can only reconstruct
a linear mapping from matching raw/calibrated pairs, labelled **inferred**, not
the original fit. Constant input, inconsistent/nonlinear data or missing raw
channels produce an explicit **unavailable** status. Current calibration is never
silently substituted for historical calibration. Other derived channels without
saved provenance also explicitly identify unavailable regressions.

### Backend setup

Python 3 is required on the backend. Install report dependencies once:

```sh
python3 scripts/setup-report-export.py
```

This creates `.venv-reports` with openpyxl and matplotlib. When GroundStation runs
from the repository root it detects this environment automatically. For a service
with another working directory, set `GS_REPORT_PYTHON` to its absolute Python
path. No display server is needed. CSV and previews need only standard Python;
missing Excel/PDF dependencies produce an actionable error.

Each job is read-only, permission-gated by ViewData, limited to two minutes and
serialized to avoid concurrent report work exhausting a Pi. The report workflow
limits a selection to 200,000 recorded rows/channel samples and 128 channels;
PDF tables are limited to 20,000 samples. Oversized selections fail explicitly,
never silently truncate. Use a narrower date range or the original streaming CSV
for large unfiltered captures. Stop recording before exporting if every final
sample must be included; active databases are read using finite snapshots.

API: `GET /api/recordings/report?request=<URL-encoded JSON>` with `recording`
(empty means all local recordings), optional `start_ms`/`end_ms`, `channels`
(null means all; each key is JSON `[sender,type,index]`), and `format`
(`preview`, `csv`, `xlsx`, `pdf`). The recording directory is always server-owned.

Validation: Rust backend tests plus
`python -m unittest discover -s tests -p test_recording_report.py` using the report
environment with `pypdf` added for PDF document validation.

## Original recorded telemetry CSV downloads

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
