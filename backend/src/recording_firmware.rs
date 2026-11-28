//! What a recording holds about the boards' firmware and health
//! (`GET /api/recordings/{id}/firmware`).
//!
//! The firmware panel shows this for a replay: which boards were heard, the
//! firmware each ran where the recording knows it, and each board's health
//! as it reported it. It is computed from the recording alone:
//!
//! - boards heard: every sender of a row, with its first and last row;
//! - health: the board's `BOARD_HEALTH` reports (raw payload rows), decoded as
//!   the live station decodes them (`network_health::decode_diagnostics`):
//!   report count, uptime at the first and last report, resets seen during
//!   the recording (uptime going backwards) and the last report in full;
//! - firmware identity: board firmware does not report its build over
//!   SEDSnet, so the recording knows it only when the data came from the
//!   firmware simulation: the capture manifest's `versions.simulated_firmware`
//!   (repository, ref, commit and SEDSnet commit per board). Otherwise each
//!   board's `firmware` is null and `identity_note` says why;
//! - provenance: the recording's first `GS_STATION_MODE` row (or the capture
//!   manifest), so hardware, firmware-sim and flight-sim data are told apart;
//! - updates: firmware transfers this station still remembers that started
//!   during the recording (`GET /api/firmware/updates`, in memory only).
//!
//! The answer has no wall-clock values, so a bundle that snapshots it
//! (`derived/firmware.json`) is reproducible.
use crate::{recording_store, state::AppState, types::Board};
use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const NO_IDENTITY: &str = "Board firmware does not report its build over SEDSnet; the recording holds each board's health reports, not its firmware version.";

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/recordings/{id}/firmware", get(firmware))
}

async fn firmware(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (_, db) = match crate::recording_replay::context::authorized_db(&state, &headers, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let root = crate::recording_export::directory(&state);
    let manifest = capture_manifest(&root, &id);
    match record(&db, &id, manifest.as_ref(), &crate::firmware_update::update_history()).await {
        Ok(value) => ([(axum::http::header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response(),
        Err(_) => (StatusCode::UNPROCESSABLE_ENTITY, "Recording has no readable telemetry table").into_response(),
    }
}

/// The capture manifest of an event capture (or of the capture an attachment sits beside).
fn capture_manifest(root: &std::path::Path, id: &str) -> Option<recording_store::Manifest> {
    let parsed = recording_store::RecordingId::parse(id)?;
    matches!(parsed, recording_store::RecordingId::Event { .. }).then_some(())?;
    recording_store::read_manifest(&root.join(parsed.relative_dir()))
}

/// The firmware-sim manifest's node name for a board.
fn sim_node(board: Board) -> Option<&'static str> {
    Some(match board {
        Board::FlightComputer => "flight",
        Board::RFBoard => "rf",
        Board::PowerBoard => "power",
        Board::GatewayBoard => "gateway",
        Board::ActuatorBoard => "actuator",
        Board::ValveBoard => "valve",
        Board::DaqBoard => "daq",
        Board::GroundStation => return None,
    })
}

fn board_of(sender: &str) -> Option<Board> {
    let sender = crate::types::canonical_sender_id(sender.trim());
    Board::ALL.iter().copied().find(|b| *b != Board::GroundStation && b.sender_id() == sender)
}

/// The 24 little-endian words of a `BOARD_HEALTH` payload (a JSON byte array).
fn health_words(payload: &str) -> Option<Vec<u32>> {
    let bytes: Vec<u8> = serde_json::from_str(payload).ok()?;
    (bytes.len() == 96).then(|| bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

#[derive(Default)]
struct Health {
    reports: u64,
    undecoded: u64,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
    first_uptime_s: Option<f64>,
    last_uptime_s: Option<f64>,
    resets_observed: u64,
    max_watchdog_missing_mask: u32,
    last: Option<crate::types::BoardDiagnostics>,
}

pub(crate) async fn record(
    db: &SqlitePool,
    id: &str,
    manifest: Option<&recording_store::Manifest>,
    updates: &[crate::firmware_update::FirmwareUpdateStatus],
) -> Result<Value, sqlx::Error> {
    let mut heard: BTreeMap<&'static str, (Board, i64, i64, i64)> = BTreeMap::new();
    for row in sqlx::query("SELECT sender_id, COUNT(*) AS n, MIN(timestamp_ms) AS first, MAX(timestamp_ms) AS last FROM telemetry GROUP BY sender_id")
        .fetch_all(db)
        .await?
    {
        let Some(board) = board_of(&row.try_get::<Option<String>, _>("sender_id")?.unwrap_or_default()) else { continue };
        let (n, first, last): (i64, i64, i64) = (row.try_get("n")?, row.try_get("first")?, row.try_get("last")?);
        let entry = heard.entry(board.sender_id()).or_insert((board, 0, first, last));
        entry.1 += n;
        entry.2 = entry.2.min(first);
        entry.3 = entry.3.max(last);
    }
    let mut health: BTreeMap<&'static str, Health> = BTreeMap::new();
    for row in sqlx::query("SELECT timestamp_ms, sender_id, payload_json FROM telemetry WHERE data_type='BOARD_HEALTH' ORDER BY id").fetch_all(db).await? {
        let Some(board) = board_of(&row.try_get::<Option<String>, _>("sender_id")?.unwrap_or_default()) else { continue };
        let at: i64 = row.try_get("timestamp_ms")?;
        let h = health.entry(board.sender_id()).or_default();
        h.reports += 1;
        let decoded = row
            .try_get::<Option<String>, _>("payload_json")?
            .and_then(|p| health_words(&p))
            .and_then(|w| crate::network_health::decode_diagnostics(&w));
        let Some(diagnostics) = decoded else {
            h.undecoded += 1;
            continue;
        };
        let uptime = diagnostics.uptime_ticks as f64 / f64::from(diagnostics.uptime_frequency_hz.max(1));
        if h.last_uptime_s.is_some_and(|previous| uptime < previous) {
            h.resets_observed += 1;
        }
        h.first_ms.get_or_insert(at);
        h.first_uptime_s.get_or_insert(uptime);
        h.last_ms = Some(at);
        h.last_uptime_s = Some(uptime);
        h.max_watchdog_missing_mask |= diagnostics.watchdog_missing_mask;
        h.last = Some(diagnostics);
    }
    let station_mode: Option<Value> = sqlx::query_scalar::<_, Option<String>>("SELECT payload_json FROM telemetry WHERE data_type='GS_STATION_MODE' ORDER BY id LIMIT 1")
        .fetch_optional(db)
        .await?
        .flatten()
        .and_then(|p| serde_json::from_str(&p).ok());
    let provenance = manifest
        .and_then(|m| m.provenance.clone())
        .or_else(|| station_mode.as_ref().and_then(|m| m["provenance"].as_str().map(str::to_owned)));
    let simulated = manifest.map(|m| &m.versions["simulated_firmware"]).filter(|v| v.is_object());
    let range = heard.values().fold(None, |acc: Option<(i64, i64)>, (_, _, first, last)| {
        Some(acc.map_or((*first, *last), |(a, b)| (a.min(*first), b.max(*last))))
    });

    let mut boards = Vec::new();
    for board in Board::ALL.iter().copied().filter(|b| *b != Board::GroundStation) {
        let identity = simulated.and_then(|s| sim_node(board).map(|node| &s["boards"][node])).filter(|v| v.is_object());
        let rows = heard.get(board.sender_id());
        if rows.is_none() && identity.is_none() {
            continue;
        }
        let firmware = identity.map(|i| {
            let mut out = json!({});
            for field in ["repo", "ref", "commit", "short", "describe", "subject", "committed", "sedsnet", "defines"] {
                if !i[field].is_null() {
                    out[field] = i[field].clone();
                }
            }
            out
        });
        let health = health.get(board.sender_id()).map(|h| {
            json!({
                "reports": h.reports, "undecoded_reports": h.undecoded,
                "first_ms": h.first_ms, "last_ms": h.last_ms,
                "first_uptime_s": h.first_uptime_s, "last_uptime_s": h.last_uptime_s,
                "resets_observed": h.resets_observed,
                "watchdog_missing_mask": h.max_watchdog_missing_mask,
                "last": h.last,
            })
        });
        boards.push(json!({
            "sender_id": board.sender_id(),
            "board_label": board.as_str(),
            "rows": rows.map_or(0, |r| r.1),
            "first_ms": rows.map(|r| r.2),
            "last_ms": rows.map(|r| r.3),
            "firmware": firmware,
            "firmware_source": if firmware.is_some() { Value::from("firmware_sim_manifest") } else { Value::Null },
            "health": health,
        }));
    }
    let updates: Vec<Value> = updates
        .iter()
        .filter(|u| range.is_some_and(|(a, b)| (a..=b).contains(&(u.started_at_ms as i64))))
        .filter_map(|u| serde_json::to_value(u).ok())
        .collect();
    let identity_known = boards.iter().any(|b| !b["firmware"].is_null());
    Ok(json!({
        "recording": id,
        "provenance": provenance,
        "station": manifest.map(|m| m.versions["groundstation_backend"].clone()).filter(|v| !v.is_null()).map(|v| json!({"groundstation_backend": v})),
        "firmware_simulator": simulated.map(|s| s["firmware_simulator"].clone()).filter(|v| !v.is_null()),
        "sedsnet": simulated.map(|s| s["sedsnet"].clone()).filter(|v| !v.is_null()),
        "identity_note": if identity_known { Value::Null } else { Value::from(NO_IDENTITY) },
        "boards": boards,
        "updates": updates,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health_payload(uptime_ms: u32, temperature_centi: i16) -> String {
        let mut words = [0u32; 24];
        words[0] = 1;
        words[1] = 0x100 | ((temperature_centi as u16 as u32) << 16);
        words[2] = uptime_ms;
        words[4] = 1000;
        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        serde_json::to_string(&bytes).unwrap()
    }

    async fn db(rows: &[(i64, &str, &str, String)]) -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER,source_timestamp_ms INTEGER,sender_id TEXT,data_type TEXT,values_json TEXT,payload_json TEXT)").execute(&db).await.unwrap();
        for (t, sender, ty, payload) in rows {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,?,?,'[]',?)")
                .bind(t).bind(sender).bind(ty).bind(payload).execute(&db).await.unwrap();
        }
        db
    }

    #[tokio::test]
    async fn health_reports_resets_and_unknown_identity_on_hardware() {
        let db = db(&[
            (0, "GS", "GS_STATION_MODE", r#"{"mode":"normal","provenance":"hardware"}"#.into()),
            (1_000, "FC", "BOARD_HEALTH", health_payload(5_000, 2_500)),
            (6_000, "FC", "BOARD_HEALTH", health_payload(10_000, 2_600)),
            (11_000, "FC", "BOARD_HEALTH", health_payload(1_000, 2_700)),
            (12_000, "VB", "VALVE_STATE", "[]".into()),
            (13_000, "VB", "BOARD_HEALTH", "[1,2,3]".into()),
        ])
        .await;
        let value = record(&db, "x.db", None, &[]).await.unwrap();
        assert_eq!(value["provenance"], "hardware");
        assert_eq!(value["identity_note"], NO_IDENTITY);
        let boards = value["boards"].as_array().unwrap();
        assert_eq!(boards.iter().map(|b| b["sender_id"].as_str().unwrap()).collect::<Vec<_>>(), ["FC", "VB"]);
        let fc = &boards[0];
        assert!(fc["firmware"].is_null());
        assert_eq!(fc["health"]["reports"], 3);
        assert_eq!(fc["health"]["resets_observed"], 1, "uptime went backwards once");
        assert_eq!(fc["health"]["first_uptime_s"], 5.0);
        assert_eq!(fc["health"]["last_uptime_s"], 1.0);
        assert_eq!(fc["health"]["last"]["die_temperature_centi_c"], 2700);
        assert_eq!(boards[1]["health"]["undecoded_reports"], 1);
        assert_eq!(boards[1]["rows"], 2);
    }

    #[tokio::test]
    async fn firmware_sim_captures_name_each_boards_build() {
        let db = db(&[(0, "FC", "FLIGHT_STATE", "[]".into())]).await;
        let manifest: recording_store::Manifest = serde_json::from_value(json!({
            "format": "seds-gs-event-capture", "version": 1, "id": "event.hitl.x.db", "event_type": "hitl", "title": "HITL run", "label": "x",
            "event_start_ms": 0, "event_end_ms": 1, "window_start_ms": 0, "window_end_ms": 1, "pre_roll_s": 0, "post_roll_s": 0,
            "station_mode": "hitl", "provenance": "firmware_sim", "simulated": true,
            "versions": {"groundstation_backend": "0.1.0", "simulated_firmware": {
                "sedsnet": {"ref": "main", "commit": "abc", "short": "abc"},
                "firmware_simulator": {"ref": "main", "commit": "def"},
                "boards": {"flight": {"repo": "Flight-Computer", "ref": "main", "commit": "0123456789", "short": "0123456", "describe": "v1.2-3-g0123456", "sedsnet": "abc", "exe": "/tmp/flight.native"}}}},
            "operator": null, "result": null, "rows": {}, "data_from_ms": 0, "data_to_ms": 1, "pre_roll_truncated": false,
            "source_segments": [], "saved_at_ms": 5
        }))
        .unwrap();
        let value = record(&db, "event.hitl.x.db", Some(&manifest), &[]).await.unwrap();
        assert_eq!(value["provenance"], "firmware_sim");
        assert!(value["identity_note"].is_null());
        assert_eq!(value["station"]["groundstation_backend"], "0.1.0");
        let fc = &value["boards"][0];
        assert_eq!(fc["firmware"]["describe"], "v1.2-3-g0123456");
        assert_eq!(fc["firmware_source"], "firmware_sim_manifest");
        assert!(fc["firmware"]["exe"].is_null(), "host paths are not reported");
        // Boards named by the manifest but not heard are still listed.
        assert_eq!(value["boards"].as_array().unwrap().len(), 1);
    }
}
