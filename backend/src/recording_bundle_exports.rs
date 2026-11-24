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
//! - `derived/review_inputs.json` (+ `models/hybrid_motor/*.json`): what the
//!   motor/flight review and its exports read, for any window
//!   (`flight_review::offline`).
//! - With `?reports=1` ([`reports`]): `exports/report.xlsx` and
//!   `exports/report.pdf`, the report studio's standard Excel and PDF reports
//!   (all channels, whole recording) as the station's report worker writes
//!   them, and `exports/reports.json` listing them or why one was left out.
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
    out.extend(crate::flight_review::offline::bundle_entries(state, id).await);
    out
}

const DEFAULT_REPORT_BUDGET_MB: u32 = 16;
const MAX_REPORT_BUDGET_MB: u32 = 64;

/// The standard Excel and PDF reports, generated at `now_ms` so a bundle made
/// at a fixed time is reproducible, within `max_mb` MiB together. A report
/// the worker cannot make (no openpyxl/matplotlib, a PDF beyond its sample
/// limit) or that does not fit is left out and `exports/reports.json` says why.
pub(crate) async fn reports(state: &Arc<AppState>, id: &str, now_ms: i64, max_mb: Option<u32>) -> Vec<(String, Vec<u8>, &'static str)> {
    let budget = (max_mb.unwrap_or(DEFAULT_REPORT_BUDGET_MB).min(MAX_REPORT_BUDGET_MB) as usize) << 20;
    let mut used = 0usize;
    let mut out = Vec::new();
    let mut index = serde_json::Map::new();
    for (format, media_type) in [("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"), ("pdf", "application/pdf")] {
        let entry = match crate::recording_report::standard(state, id, format, now_ms).await {
            Ok(bytes) if used + bytes.len() <= budget => {
                used += bytes.len();
                let path = format!("exports/report.{format}");
                let entry = json!({"path": path, "filename": format!("telemetry-report.{format}"), "bytes": bytes.len(), "generated_ms": now_ms});
                out.push((path, bytes, media_type));
                entry
            }
            Ok(bytes) => json!({"skipped": format!("{} bytes exceed the report budget of {budget} bytes", bytes.len())}),
            Err((status, message)) => json!({"skipped": message, "status": status.as_u16()}),
        };
        index.insert(format.to_owned(), entry);
    }
    out.push(("exports/reports.json".to_owned(), serde_json::to_vec_pretty(&Value::Object(index)).unwrap_or_default(), "application/json"));
    out
}

/// A query flag: `true`/`1`/`yes`/`on` (any case) or `false`/`0`/`no`/`off`.
pub(crate) fn flag<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    let text = <String as serde::Deserialize>::deserialize(deserializer)?;
    match text.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" | "" => Ok(false),
        other => Err(serde::de::Error::custom(format!("expected true or false, not {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn report_option_parses_as_a_query_flag() {
        for (query, expected) in [("reports=true", true), ("reports=1", true), ("reports=false", false), ("tiles=true", false)] {
            let parse = |query: &str| axum::extract::Query::<crate::recording_bundle::Options>::try_from_uri(&format!("/x?{query}").parse().unwrap());
            assert_eq!(parse(query).unwrap().reports, expected, "{query}");
        }
        assert!(axum::extract::Query::<crate::recording_bundle::Options>::try_from_uri(&"/x?reports=maybe".parse().unwrap()).is_err());
    }
}
