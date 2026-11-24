//! Replay-bundle entries that let a client produce the station's exports of a
//! recording offline (`docs/backend/replay-bundle.md`, "Exports offline").
//!
//! - `config/report_context.json`: the report studio's export-time context
//!   (`recording_report::context`: valve labels, layout units, recorded
//!   diagnostic stream schema), which the station adds to every report request.
//! - `derived/calibration_history.json`: the recording's `calibration_history`
//!   rows (`recording_report::calibration_history`), the report's recorded
//!   calibration epochs. Absent when the recording has no such table.
//! - `exports/motor_export.zip`, `exports/flight_export.zip`: the motor and
//!   flight review exports without a range (`flight_review::bundle_exports`),
//!   generated at the bundle's time and without raster charts; only those
//!   that apply to the recording.
//! - `exports/index.json`: which exports the bundle holds, with the file name
//!   and window of each.
//!
//! The recording CSV is already `data/telemetry.csv`; the time-range CSV and
//! the report's preview, CSV and DAQ SD layouts are derived from it.
use crate::state::AppState;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) async fn entries(state: &Arc<AppState>, id: &str, now_ms: i64) -> Vec<(String, Vec<u8>, &'static str)> {
    let pretty = |value: &Value| serde_json::to_vec_pretty(value).unwrap_or_default();
    let mut out = vec![("config/report_context.json".to_owned(), pretty(&crate::recording_report::context()), "application/json")];
    let root = crate::recording_export::directory(state);
    if let Ok(path) = crate::recording_export::recording_path(&root, id).await
        && let Some(history) = crate::recording_report::calibration_history(&path).await
    {
        out.push(("derived/calibration_history.json".to_owned(), pretty(&history), "application/json"));
    }
    let mut index = serde_json::Map::new();
    for export in crate::flight_review::bundle_exports(state, id, now_ms).await {
        let path = format!("exports/{}.zip", export.kind);
        index.insert(
            export.kind.to_owned(),
            json!({"path": path, "filename": export.filename, "from_ms": export.from_ms, "to_ms": export.to_ms, "generated_ms": now_ms, "raster": false}),
        );
        out.push((path, export.bytes, "application/zip"));
    }
    out.push(("exports/index.json".to_owned(), pretty(&Value::Object(index)), "application/json"));
    out
}
