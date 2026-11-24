//! Self-contained replay bundles (`<recording>.gsreplay`).
//!
//! `GET /api/recordings/{id}/bundle` packs everything a client needs to
//! replay a recording with no station. That is the recorded rows, the
//! configuration that labels them, the vehicle geometry, derived products and,
//! optionally, map tiles around the flight track. It is one ZIP file with a
//! `manifest.json`.
//!
//! Every entry is the body this station's own route returns for that
//! resource, fetched through the same handlers and with the caller's
//! credentials. So a bundle is a snapshot of the contract
//! (`docs/backend/replay-bundle.md` and the UI's `docs/replay-bundle.md`).
//! `data/telemetry.csv` is byte for byte the recording's
//! `GET /api/recordings/{id}/csv`, so a client can hand it out as the CSV
//! export with no station.
use crate::{auth::Permission, state::AppState};
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

pub(crate) const FORMAT: &str = "gsreplay";
pub(crate) const VERSION: u64 = 1;
pub(crate) const MEDIA_TYPE: &str = "application/vnd.seds.gsreplay+zip";
const DEFAULT_TILE_BUDGET_MB: u32 = 8;
const MAX_TILE_BUDGET_MB: u32 = 32;
/// Largest body read from one internal route (the replay export caps itself at 64 MiB).
const MAX_ENTRY_BYTES: usize = 96 << 20;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Options {
    /// Include cached map tiles around the recorded track.
    #[serde(default)]
    pub tiles: bool,
    /// Tile budget in MiB (default 8, at most 32).
    pub tiles_max_mb: Option<u32>,
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/recordings/{id}/bundle", get(download))
}

async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(options): Query<Options>,
) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    if !crate::recording_export::valid_id(&id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match build(state, &headers, &id, &options, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, MEDIA_TYPE.to_owned()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}.gsreplay\"", id.trim_end_matches(".db"))),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            bytes,
        )
            .into_response(),
        Err((status, message)) => (status, message).into_response(),
    }
}

/// The station routes a bundle snapshots, with their real handlers.
fn sources(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(crate::recording_export::routes())
        .merge(crate::diagrams::routes())
        .merge(crate::vehicle_geometry::routes())
        .merge(crate::flight_review::routes())
        .merge(crate::burn_report::routes())
        .merge(crate::fire_archive::routes())
        .merge(crate::apogee_planner::routes())
        .merge(crate::web::bundle_config_routes())
        .with_state(state)
}

async fn fetch(router: &Router, headers: &HeaderMap, path: &str) -> Result<Vec<u8>, u16> {
    let mut request = Request::get(path);
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        request = request.header(header::AUTHORIZATION, auth.clone());
    }
    let response = router.clone().oneshot(request.body(Body::empty()).map_err(|_| 400u16)?).await.map_err(|_| 500u16)?;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), MAX_ENTRY_BYTES).await.map_err(|_| 507u16)?;
    if status.is_success() { Ok(body.to_vec()) } else { Err(status.as_u16()) }
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Slippy-map tile of a coordinate.
fn tile_of(lat: f64, lon: f64, z: u32) -> (u32, u32) {
    let n = f64::from(1u32 << z);
    let x = ((lon + 180.0) / 360.0 * n).floor().clamp(0.0, n - 1.0) as u32;
    let rad = lat.clamp(-85.05, 85.05).to_radians();
    let y = ((1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n).floor().clamp(0.0, n - 1.0) as u32;
    (x, y)
}

struct Entries {
    files: Vec<(String, Vec<u8>, &'static str)>,
    missing: Vec<Value>,
}

impl Entries {
    fn add(&mut self, path: &str, bytes: Vec<u8>, media_type: &'static str) {
        self.files.push((path.to_owned(), bytes, media_type));
    }
    fn add_json(&mut self, path: &str, value: &Value) {
        self.add(path, serde_json::to_vec_pretty(value).unwrap_or_default(), "application/json");
    }
    /// An optional resource: kept when the station serves it, listed as missing otherwise.
    async fn optional(&mut self, router: &Router, headers: &HeaderMap, route: &str, path: &str) -> Option<Value> {
        match fetch(router, headers, route).await {
            Ok(bytes) => {
                let value = serde_json::from_slice::<Value>(&bytes).ok();
                self.add(path, bytes, "application/json");
                value
            }
            Err(status) => {
                self.missing.push(json!({"path": path, "route": route, "status": status}));
                None
            }
        }
    }
}

pub(crate) async fn build(
    state: Arc<AppState>,
    headers: &HeaderMap,
    id: &str,
    options: &Options,
    now_ms: i64,
) -> Result<Vec<u8>, (StatusCode, String)> {
    let router = sources(state);
    let required = |route: String| {
        let router = router.clone();
        async move {
            fetch(&router, headers, &route).await.map_err(|status| {
                (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), format!("{route} answered {status}"))
            })
        }
    };
    let mut replay: Value = serde_json::from_slice(&required(format!("/api/recordings/{id}/replay/export")).await?)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let csv = required(format!("/api/recordings/{id}/csv")).await?;
    let mut entries = Entries { files: Vec::new(), missing: Vec::new() };

    let context = match replay.get("replay_context").filter(|c| c.is_object()) {
        Some(context) => Some(context.clone()),
        None => entries.optional(&router, headers, &format!("/api/recordings/{id}/replay/context"), "derived/context.json").await,
    };
    let view = context.as_ref().and_then(|c| c["default_view"].as_str()).unwrap_or("normal").to_owned();
    let layout = match entries.optional(&router, headers, &format!("/api/recordings/{id}/replay/layout?view={view}"), "config/layout.json").await {
        Some(layout) => Some(layout),
        None => entries.optional(&router, headers, "/api/layout", "config/layout.json").await,
    };
    let boards = entries.optional(&router, headers, "/api/boards", "config/boards.json").await;
    let geometry = entries.optional(&router, headers, crate::vehicle_geometry::PATH, "models/vehicle_geometry.json").await;
    entries.optional(&router, headers, "/api/calibration", "config/calibration.json").await;
    entries.optional(&router, headers, "/api/calibration_config", "config/calibration_config.json").await;
    entries.optional(&router, headers, "/api/diagrams", "config/diagrams.json").await;
    let map_config = entries.optional(&router, headers, "/api/map_config", "config/map_config.json").await;
    entries.optional(&router, headers, &format!("/api/recordings/{id}/replay/alerts"), "derived/alerts.json").await;
    entries.optional(&router, headers, &format!("/api/flight_review/recording/{id}"), "derived/flight_review.json").await;
    entries.optional(&router, headers, "/api/apogee_planner", "derived/apogee_planner.json").await;
    let upper = replay["snapshot"]["upper_id"].as_i64().unwrap_or(0);
    let trajectory = entries
        .optional(&router, headers, &format!("/api/recordings/{id}/replay/trajectory?upper_id={upper}"), "derived/trajectory.json")
        .await;
    // Burn reports and fire-archive entries made from this recording.
    let stem = id.trim_end_matches(".db");
    for (route, path) in [("/api/burn_reports", "derived/burn_reports.json"), ("/api/fire_archive", "derived/fire_archive.json")] {
        if let Ok(bytes) = fetch(&router, headers, route).await
            && let Ok(list) = serde_json::from_slice::<Value>(&bytes)
        {
            let items = list.as_array().cloned().or_else(|| list.as_object().and_then(|o| o.values().find_map(|v| v.as_array().cloned()))).unwrap_or_default();
            let related: Vec<Value> = items.into_iter().filter(|item| item.to_string().contains(stem)).collect();
            if !related.is_empty() {
                entries.add_json(path, &Value::Array(related));
            }
        }
    }

    // The replay document carries what a client needs even if it reads nothing else.
    if replay["layout"].is_null() && let Some(layout) = &layout {
        replay["layout"] = layout.clone();
    }
    if replay["boards"].is_null() && let Some(boards) = &boards {
        replay["boards"] = boards["boards"].clone();
    }
    if replay["vehicle_geometry"].is_null() && let Some(geometry) = &geometry {
        replay["vehicle_geometry"] = geometry.clone();
    }
    if replay["mode"].is_null() {
        let test_fire = context.as_ref().and_then(|c| c["test_fire"].as_bool()).unwrap_or(false);
        replay["mode"] = if test_fire { "test_fire" } else { "flight" }.into();
    }
    if replay["configuration_provenance"].is_null() {
        replay["configuration_provenance"] = "Station configuration at bundle export; not a claim of historical capture-time configuration.".into();
    }
    let rows = replay["samples"].as_array().map_or(0, Vec::len);

    // Optional tiles around the track, lowest zoom first, within the budget.
    let mut tiles = json!({"included": 0, "bytes": 0, "budget_bytes": 0, "complete": false});
    if options.tiles
        && let Some(points) = trajectory.as_ref().and_then(|t| t["points"].as_array())
        && !points.is_empty()
    {
        let budget = (options.tiles_max_mb.unwrap_or(DEFAULT_TILE_BUDGET_MB).min(MAX_TILE_BUDGET_MB) as usize) << 20;
        let max_zoom = map_config.as_ref().and_then(|m| m["max_native_zoom"].as_u64()).unwrap_or(16).min(17) as u32;
        let (mut la0, mut la1, mut lo0, mut lo1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in points {
            let (Some(la), Some(lo)) = (p["latitude"].as_f64(), p["longitude"].as_f64()) else { continue };
            (la0, la1, lo0, lo1) = (la0.min(la), la1.max(la), lo0.min(lo), lo1.max(lo));
        }
        let (mut used, mut count, mut complete) = (0usize, 0u64, true);
        'zoom: for z in 10..=max_zoom {
            let (x0, y0) = tile_of(la1, lo0, z);
            let (x1, y1) = tile_of(la0, lo1, z);
            for x in x0.saturating_sub(1)..=x1 + 1 {
                for y in y0.saturating_sub(1)..=y1 + 1 {
                    let Ok(bytes) = fetch(&router, headers, &format!("/tiles/{z}/{x}/{y}.jpg")).await else { continue };
                    if bytes.is_empty() {
                        continue;
                    }
                    if used + bytes.len() > budget {
                        complete = false;
                        break 'zoom;
                    }
                    used += bytes.len();
                    count += 1;
                    entries.add(&format!("tiles/{z}/{x}/{y}.jpg"), bytes, "image/jpeg");
                }
            }
        }
        tiles = json!({"included": count, "bytes": used, "budget_bytes": budget, "complete": complete});
    }

    entries.files.insert(0, ("data/telemetry.csv".to_owned(), csv, "text/csv"));
    entries.files.insert(0, ("replay.json".to_owned(), serde_json::to_vec(&replay).unwrap_or_default(), "application/json"));
    let manifest = json!({
        "format": FORMAT,
        "version": VERSION,
        "created_ms": now_ms,
        "generator": format!("groundstation_backend {}", env!("CARGO_PKG_VERSION")),
        "source": {"station_name": crate::host_power::station_name(), "recording": id},
        "mode": replay["mode"],
        "recorded_mode": context.as_ref().map(|c| c["recorded_mode"].clone()).unwrap_or(Value::Null),
        "time_range": {"start_ms": replay["snapshot"]["start_ms"], "end_ms": replay["snapshot"]["end_ms"]},
        "rows": rows,
        "tiles": tiles,
        "missing": entries.missing,
        "entries": entries.files.iter().map(|(path, bytes, media_type)| json!({
            "path": path, "bytes": bytes.len(), "sha256": sha256_hex(bytes), "media_type": media_type,
        })).collect::<Vec<_>>(),
    });
    let mut zip = crate::flight_review::zip::Zip::new(now_ms);
    zip.add("manifest.json", &serde_json::to_vec_pretty(&manifest).unwrap_or_default());
    for (path, bytes, _) in &entries.files {
        zip.add(path, bytes);
    }
    Ok(zip.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizes for a realistic ten-minute capture (about 150,000 rows at
    /// flight rates). Run with `--ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn realistic_capture_bundle_size() {
        let root = crate::recording_export::tests::TestDirectory::new();
        let state = crate::recording_export::tests::export_state(&root.0, true).await;
        let id = "groundstation_recording_2026-01-02_00-00-00_000.db";
        let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(id)).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        let mut rows = 0usize;
        for ms in (0..600_000i64).step_by(10) {
            let t = ms as f64 / 1000.0;
            let put = |sender: &str, ty: &str, values: String| (sender.to_owned(), ty.to_owned(), values);
            let mut batch = vec![put("FC", "ACCEL_DATA", format!("[{:.3},{:.3},{:.3}]", t.sin(), t.cos(), 9.81 + (t * 3.0).sin()))];
            if ms % 20 == 0 {
                batch.push(put("DAQ", "LOADCELL_50_WEIGHT_KG", format!("[{:.3}]", 10.0 * (t / 600.0))));
                batch.push(put("VB", "OX_TANK_PRESSURE", format!("[{:.1}]", 700.0 + 10.0 * t.sin())));
                batch.push(put("FC", "BAROMETER_DATA", format!("[{:.1},{:.1},{:.1}]", 100.0 * t, 90000.0 - t, 25.0)));
            }
            if ms % 200 == 0 {
                batch.push(put("FC", "GPS_DATA", format!("[{:.7},{:.7},{:.1}]", 31.758 + t * 1e-5, -102.046 + t * 1e-5, 900.0 + t)));
            }
            if ms % 1000 == 0 {
                batch.push(put("VB", "VALVE_STATE", "[0,1,0,0,0,0,1]".into()));
                batch.push(put("PB", "BATTERY_VOLTAGE", format!("[{:.2}]", 7.9 - t * 1e-4)));
            }
            for (sender, ty, values) in batch {
                sqlx::query("INSERT INTO telemetry(timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,?,?,?,'[]')")
                    .bind(1_767_225_600_000 + ms).bind(sender).bind(ty).bind(values).execute(&mut *tx).await.unwrap();
                rows += 1;
            }
        }
        tx.commit().await.unwrap();
        let headers = HeaderMap::new();
        let started = std::time::Instant::now();
        let bundle = build(state, &headers, id, &Options::default(), 0).await.unwrap();
        let db_bytes = std::fs::metadata(root.0.join(id)).map(|m| m.len()).unwrap_or(0);
        println!("realistic_capture_bundle_size: {rows} rows, recording db {db_bytes} bytes, bundle {} bytes, built in {:?}", bundle.len(), started.elapsed());
        assert!(bundle.len() < 64 << 20);
    }

    #[test]
    fn tiles_cover_the_coordinate() {
        assert_eq!(tile_of(0.0, 0.0, 1), (1, 1));
        assert_eq!(tile_of(31.758, -102.046, 14), (3547, 6666));
    }
}
