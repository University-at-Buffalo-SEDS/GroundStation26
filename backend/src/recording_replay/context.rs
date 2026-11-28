//! The recorded context a replay presents: the station mode (and its changes)
//! the data was recorded in, its provenance, the views and bodies that mode
//! shows, the recorded mode's layout and the burn-report thrust overlay.
//!
//! Everything comes from the recording itself (GS_STATION_MODE / GS_IGT rows,
//! the capture manifest, the burn report and fire archive files). The live
//! station mode is reported for information only and is never changed.
use super::sources::{self, Source};
use crate::{auth::Permission, station_mode::Mode, state::AppState};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeSet;
use std::sync::Arc;

const MAX_TIMELINE: usize = 64;
const MAX_TYPES: usize = 512;
/// Senders on the test stand (and the station) in an integrated ground test.
pub(crate) const STAND_SENDERS: [&str; 7] = ["GS", "DAQ", "VB", "AB", "GB", "DAQ_SD", "PICO"];
/// The rocket's avionics, on the ground beside the stand.
pub(crate) const ROCKET_SENDERS: [&str; 4] = ["FC", "RF", "PB", "FC_SD"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModeSpan {
    pub from_ms: i64,
    pub mode: String,
    pub igt_active: bool,
    pub hitl_replay: bool,
    pub mock_wet_dress: bool,
    pub provenance: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Body {
    pub key: String,
    pub label: String,
    pub senders: Vec<String>,
    /// Data types in this body that are replayed (simulated), not measured.
    pub simulated_types: Vec<String>,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Series {
    pub data_type: String,
    pub sender_id: String,
    pub rows: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Context {
    pub replay: bool,
    pub read_only: bool,
    pub recording: String,
    pub source_kind: String,
    pub capture_id: Option<String>,
    /// Canonical mode id (`normal`, `hitl`, `test_fire`, `testing`,
    /// `integrated_ground_test`) or `unknown`.
    pub recorded_mode: String,
    /// `recording_rows`, `manifest`, `inferred` or `none`.
    pub mode_source: String,
    pub mode_timeline: Vec<ModeSpan>,
    /// `hardware`, `firmware_sim`, `internal_flight_sim`, `simulated` or `unknown`.
    pub provenance: String,
    pub simulated: bool,
    pub firmware_sim: bool,
    pub hitl: bool,
    pub hitl_replay: bool,
    pub igt: bool,
    pub test_fire: bool,
    /// The view (mode layout) the replay opens in, and the views it offers.
    pub default_view: String,
    pub view_options: Vec<String>,
    pub tabs: Vec<String>,
    pub bodies: Vec<Body>,
    pub series: Vec<Series>,
    pub sim_sample_rows: i64,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub upper_id: i64,
    pub compatibility: Value,
    pub manifest: Option<Value>,
    pub board_sd: Vec<Value>,
    pub burn_report: Option<Value>,
    /// The station's mode now, for information: replay never changes it.
    pub live_station_mode: String,
}

pub(crate) fn mode_id(mode: Mode) -> &'static str {
    crate::station_mode_switch::mode_id(mode)
}
fn canonical(id: &str) -> Option<&'static str> {
    crate::runtime_mode::parse(id).map(mode_id)
}

/// The tabs a replay in `view` shows (see the frontend's saved replay view).
pub(crate) fn tabs_for(view: &str) -> Vec<String> {
    let list: &[&str] = match view {
        "test_fire" => &["overview", "data", "gse", "thrust", "alerts"],
        "integrated_ground_test" => {
            &["overview", "stand", "rocket", "data", "map", "estimator", "recovery", "thrust", "alerts"]
        }
        "hitl" => &["overview", "data", "map", "estimator", "recovery", "hitl", "alerts"],
        _ => &["overview", "data", "map", "estimator", "recovery", "alerts"],
    };
    list.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn igt_bodies() -> Vec<Body> {
    vec![
        Body {
            key: "stand".into(),
            label: "Stand (real)".into(),
            senders: STAND_SENDERS.iter().map(|s| s.to_string()).collect(),
            simulated_types: Vec::new(),
            note: "Motor and tanks on the test stand: fill, load cells, chamber PT, valves and the burn report. Real measurements.".into(),
        },
        Body {
            key: "rocket".into(),
            label: "Rocket flight (replayed)".into(),
            senders: ROCKET_SENDERS.iter().map(|s| s.to_string()).collect(),
            simulated_types: vec!["SIM_SAMPLE".into()],
            note: "Avionics on the ground without the motor: the FC flies a replayed flight (SIM_SAMPLE rows, SIMULATED) with its estimator and recovery events; at-rest sensors, GPS and pyro dry-run/live events are real.".into(),
        },
    ]
}

/// Parses one GS_STATION_MODE payload.
fn span(timestamp_ms: i64, payload: &str) -> Option<ModeSpan> {
    let v: Value = serde_json::from_str(payload).ok()?;
    let mode = canonical(v["mode"].as_str()?)?.to_owned();
    Some(ModeSpan {
        from_ms: timestamp_ms,
        mode,
        igt_active: v["igt_active"].as_bool().unwrap_or(false),
        hitl_replay: v["hitl_replay"].as_bool().unwrap_or(false),
        mock_wet_dress: v["mock_wet_dress"].as_bool().unwrap_or(false),
        provenance: v["provenance"].as_str().map(str::to_owned),
    })
}

/// Builds the context of the replay database `db` for `source`.
pub(crate) async fn context_from(
    root: &std::path::Path,
    id: &str,
    source: &Source,
    db: &SqlitePool,
) -> anyhow::Result<Context> {
    let snapshot = super::snapshot_from(db).await?;
    let mut timeline: Vec<ModeSpan> = Vec::new();
    let rows = sqlx::query("SELECT timestamp_ms,payload_json FROM telemetry WHERE data_type='GS_STATION_MODE' ORDER BY timestamp_ms,id LIMIT 1024")
        .fetch_all(db)
        .await?;
    for row in rows {
        let ts: i64 = row.try_get("timestamp_ms")?;
        let payload: Option<String> = row.try_get("payload_json")?;
        if let Some(next) = payload.as_deref().and_then(|p| span(ts, p)) {
            let same = timeline.last().is_some_and(|last| {
                last.mode == next.mode
                    && last.igt_active == next.igt_active
                    && last.hitl_replay == next.hitl_replay
                    && last.provenance == next.provenance
            });
            if !same && timeline.len() < MAX_TIMELINE {
                timeline.push(next);
            }
        }
    }
    let series_rows = sqlx::query("SELECT data_type,COALESCE(sender_id,'') AS sender_id,COUNT(*) AS n FROM telemetry GROUP BY data_type,sender_id ORDER BY data_type,sender_id LIMIT ?")
        .bind(MAX_TYPES as i64)
        .fetch_all(db)
        .await?;
    let mut series: Vec<Series> = Vec::new();
    for row in series_rows {
        // Legacy type names (pre-rename recordings) list under their canonical name.
        let data_type: String = row.try_get("data_type")?;
        let data_type = crate::telemetry_schema::canonical_type_name(&data_type).to_string();
        let sender_id: String = row.try_get("sender_id")?;
        let rows: i64 = row.try_get("n")?;
        match series.iter_mut().find(|s| s.data_type == data_type && s.sender_id == sender_id) {
            Some(existing) => existing.rows += rows,
            None => series.push(Series { data_type, sender_id, rows }),
        }
    }
    series.sort_by(|a, b| (&a.data_type, &a.sender_id).cmp(&(&b.data_type, &b.sender_id)));
    let count = |ty: &str| series.iter().filter(|s| s.data_type == ty).map(|s| s.rows).sum::<i64>();
    let sim_sample_rows = count("SIM_SAMPLE");

    let capture_id = source.capture_id().or_else(|| match source {
        Source::Fire(fire) => crate::fire_archive::get(root, fire).and_then(|r| r.capture_id),
        _ => None,
    });
    let folder = match &capture_id {
        Some(c) => sources::capture_folder(root, c).await,
        None => None,
    };
    let manifest = folder.as_deref().and_then(crate::recording_store::read_manifest);

    let (recorded_mode, mode_source) = if let Some(first) = timeline.first() {
        (first.mode.clone(), "recording_rows")
    } else if let Some(mode) = manifest.as_ref().and_then(|m| canonical(&m.station_mode)) {
        (mode.to_owned(), "manifest")
    } else {
        // Recordings made before the GS_STATION_MODE row: infer from content.
        let has = |sender: &str| series.iter().any(|s| crate::types::canonical_sender_id(&s.sender_id) == sender);
        if sim_sample_rows > 0 {
            ("integrated_ground_test".to_owned(), "inferred")
        } else if series.is_empty() {
            ("unknown".to_owned(), "none")
        } else if !has("FC") && count("LOADCELL_WEIGHT_KG") + count("KG1000") > 0 {
            ("test_fire".to_owned(), "inferred")
        } else {
            ("normal".to_owned(), "inferred")
        }
    };
    // Any part of the recording in the integrated ground test (or HITL with
    // its replay) makes it a two-body replay.
    let igt = recorded_mode == "integrated_ground_test"
        || timeline.iter().any(|s| s.mode == "integrated_ground_test" || s.igt_active)
        || (timeline.is_empty() && sim_sample_rows > 0);
    let hitl = recorded_mode == "hitl" || timeline.iter().any(|s| s.mode == "hitl");
    let hitl_replay = timeline.iter().any(|s| s.hitl_replay && s.mode == "hitl");
    let test_fire = recorded_mode == "test_fire";
    let provenance = timeline
        .iter()
        .find_map(|s| s.provenance.clone())
        .or_else(|| manifest.as_ref().and_then(|m| m.provenance.clone()))
        .unwrap_or_else(|| if manifest.is_some() || !timeline.is_empty() { "hardware".into() } else { "unknown".into() });
    let firmware_sim = provenance == "firmware_sim";
    let simulated = !matches!(provenance.as_str(), "hardware" | "unknown") || hitl;
    // HITL simulates any configuration: open it in the configuration it ran
    // (the ground-test replay -> IGT view), else the HITL layout.
    let default_view = if hitl && hitl_replay {
        "integrated_ground_test".to_owned()
    } else if recorded_mode == "unknown" {
        "normal".to_owned()
    } else {
        recorded_mode.clone()
    };
    let view_options = ["normal", "test_fire", "testing", "hitl", "integrated_ground_test"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Compatibility notes for older recordings.
    let mut health_versions = BTreeSet::new();
    let health = sqlx::query("SELECT payload_json FROM telemetry WHERE data_type='DAQ_INPUT_HEALTH' AND payload_json IS NOT NULL GROUP BY length(payload_json) LIMIT 16")
        .fetch_all(db)
        .await?;
    for row in health {
        let payload: Option<String> = row.try_get("payload_json")?;
        if let Some(words) = payload.as_deref().and_then(super::payload_words) {
            health_versions.insert(words.first().map(|w| w & 0xFF).unwrap_or(0));
        }
    }
    let volts_from = crate::telemetry_db::loadcell_volts_from_id(db).await;
    let loadcell_units = if volts_from <= 1 || snapshot.upper_id == 0 {
        "adc_volts"
    } else if volts_from > snapshot.upper_id {
        "legacy_raw_converted"
    } else {
        "mixed_legacy_raw_converted"
    };
    let compatibility = json!({
        "barometer_order": "alt_prs_tmp",
        "barometer_note": "BAROMETER_DATA values are in the FC's order (altitude, pressure, temperature) in every recording; layouts saved before the label fix are relabelled.",
        "daq_input_health_versions": health_versions.into_iter().collect::<Vec<_>>(),
        "loadcell_units": loadcell_units,
        "board_sd_time": "network",
    });

    let board_sd: Vec<Value> = manifest
        .as_ref()
        .map(|m| m.board_sd.iter().filter_map(|e| serde_json::to_value(e).ok()).collect())
        .unwrap_or_default();
    let burn_report = folder
        .as_deref()
        .and_then(crate::burn_report::read_report)
        .map(|r| json!({"id": r.id, "status": r.status, "simulated": r.simulated, "mode": r.mode,
                        "ignition_ms": r.ignition_ms, "total_impulse_ns": r.total_impulse_ns,
                        "peak_thrust_n": r.peak_thrust_n, "burn_time_s": r.burn_time_s}));
    let view = default_view.clone();
    Ok(Context {
        replay: true,
        read_only: true,
        recording: id.to_owned(),
        source_kind: source.kind().to_owned(),
        capture_id,
        bodies: if igt { igt_bodies() } else { Vec::new() },
        tabs: tabs_for(&view),
        recorded_mode,
        mode_source: mode_source.to_owned(),
        mode_timeline: timeline,
        provenance,
        simulated,
        firmware_sim,
        hitl,
        hitl_replay,
        igt,
        test_fire,
        default_view,
        view_options,
        series,
        sim_sample_rows,
        start_ms: snapshot.start_ms,
        end_ms: snapshot.end_ms,
        upper_id: snapshot.upper_id,
        compatibility,
        manifest: manifest.and_then(|m| serde_json::to_value(m).ok()),
        board_sd,
        burn_report,
        live_station_mode: mode_id(crate::runtime_mode::current()).to_owned(),
    })
}

fn no_store(value: Value) -> Response {
    super::json(value)
}

pub(crate) async fn authorized_db(state: &Arc<AppState>, headers: &HeaderMap, id: &str) -> Result<(Source, SqlitePool), Response> {
    crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    let source = Source::parse(id).ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?;
    let db = sources::open(state, id).await.map_err(|code| {
        (code, match code {
            StatusCode::NOT_FOUND => "Recording not found. Refresh the recording list.",
            StatusCode::PAYLOAD_TOO_LARGE => "The selected range is too large to replay; pick a shorter range.",
            StatusCode::BAD_REQUEST => "Invalid replay source.",
            _ => "Recording cannot be replayed.",
        })
            .into_response()
    })?;
    Ok((source, db))
}

pub async fn context(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (source, db) = match authorized_db(&state, &headers, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let root = crate::recording_export::directory(&state);
    match context_from(&root, &id, &source, &db).await {
        Ok(ctx) => no_store(serde_json::to_value(ctx).unwrap_or(Value::Null)),
        Err(_) => (StatusCode::UNPROCESSABLE_ENTITY, "Recording has no readable telemetry table").into_response(),
    }
}

#[derive(Deserialize)]
pub struct ViewQuery {
    #[serde(default)]
    pub view: Option<String>,
}

/// The layout of `view` prepared for a replay: no actions (replay never
/// commands), marked read only, its mode flags set for that view.
pub(crate) fn replay_layout(view: &str) -> Result<Value, String> {
    let mode = crate::runtime_mode::parse(view).ok_or("Unknown replay view")?;
    let layout = crate::layout::load_layout_for_mode(mode)?;
    let mut value = serde_json::to_value(layout).map_err(|e| e.to_string())?;
    value["actions_tab"]["actions"] = json!([]);
    value["test_fire_mode"] = (mode == Mode::TestFire).into();
    value["igt_mode"] = (mode == Mode::IntegratedGroundTest).into();
    crate::layout::relabel_legacy_barometer(&mut value);
    value["replay"] = json!({"view": mode_id(mode), "read_only": true, "tabs": tabs_for(mode_id(mode))});
    Ok(value)
}

pub async fn layout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<ViewQuery>,
) -> Response {
    let (source, db) = match authorized_db(&state, &headers, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let view = match query.view.filter(|v| !v.is_empty()) {
        Some(v) => match canonical(&v) {
            Some(v) => v.to_owned(),
            None => return (StatusCode::BAD_REQUEST, "Unknown replay view").into_response(),
        },
        None => {
            let root = crate::recording_export::directory(&state);
            match context_from(&root, &id, &source, &db).await {
                Ok(ctx) => ctx.default_view,
                Err(_) => "normal".into(),
            }
        }
    };
    match replay_layout(&view) {
        Ok(value) => no_store(value),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// The burn report thrust curve (absolute network ms, N) for a capture-based
/// replay, or a fire archive entry's curve, to draw over the replay.
pub(crate) async fn overlays_from(root: &std::path::Path, source: &Source) -> Value {
    let mut thrust = Value::Null;
    let capture = source.capture_id();
    if let Some(folder) = match &capture {
        Some(c) => sources::capture_folder(root, c).await,
        None => None,
    } && let Some(report) = crate::burn_report::read_report(&folder)
    {
        thrust = json!({"source": "burn_report", "id": report.id, "status": report.status,
            "simulated": report.simulated, "ignition_ms": report.ignition_ms,
            "peak_thrust_n": report.peak_thrust_n, "total_impulse_ns": report.total_impulse_ns,
            "points": report.curve.iter().map(|p| json!([report.ignition_ms + (p[0] * 1000.0).round() as i64, p[1]])).collect::<Vec<_>>()});
    }
    let mut fires = Vec::new();
    for record in crate::fire_archive::load_all(root) {
        let linked = match source {
            Source::Fire(id) => &record.id == id,
            _ => capture.is_some() && record.capture_id == capture,
        };
        if !linked {
            continue;
        }
        if thrust.is_null()
            && let Some(curve) = crate::fire_archive::curve(root, &record.id)
        {
            let t0 = sources::fire_t0(root, &record);
            thrust = json!({"source": "fire_archive", "id": record.id, "simulated": record.simulated,
                "ignition_ms": t0, "peak_thrust_n": record.peak_thrust_n, "total_impulse_ns": record.impulse_ns,
                "points": curve.iter().map(|p| json!([t0 + (p[0] * 1000.0).round() as i64, p[1]])).collect::<Vec<_>>()});
        }
        fires.push(json!({"id": record.id, "label": record.label, "date": record.date,
            "simulated": record.simulated, "review": record.review.status}));
    }
    json!({"thrust": thrust, "fire_archive": fires})
}

pub async fn overlays(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return r;
    }
    let Some(source) = Source::parse(&id) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let root = crate::recording_export::directory(&state);
    no_store(overlays_from(&root, &source).await)
}

/// The alerts the station raised while recording (at most 500, oldest
/// first). They are history: a replay shows them labelled as recorded and
/// never raises, acknowledges or notifies them.
pub(crate) async fn alerts_from(db: &SqlitePool) -> anyhow::Result<Value> {
    let rows = sqlx::query("SELECT timestamp_ms,severity,message FROM alerts ORDER BY timestamp_ms,id LIMIT 501")
        .fetch_all(db)
        .await?;
    let truncated = rows.len() > 500;
    let mut alerts = Vec::new();
    for row in rows.into_iter().take(500) {
        let message: String = row.try_get("message")?;
        alerts.push(json!({"timestamp_ms": row.try_get::<i64, _>("timestamp_ms")?,
            "severity": row.try_get::<String, _>("severity")?,
            "message": message.chars().take(512).collect::<String>(), "recorded": true}));
    }
    Ok(json!({"alerts": alerts, "truncated": truncated}))
}

pub async fn alerts(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (_, db) = match authorized_db(&state, &headers, &id).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match alerts_from(&db).await {
        Ok(v) => no_store(v),
        // A file without an alerts table simply has none.
        Err(_) => no_store(json!({"alerts": [], "truncated": false})),
    }
}
