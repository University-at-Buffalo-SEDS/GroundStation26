use crate::layout;
use crate::loadcell;
use crate::map::{DEFAULT_MAP_REGION, detect_max_native_zoom, tile_bundle_path};
use crate::sequences::{ActionPolicyMsg, PersistentNotification};
use crate::state::AppState;
use axum::http::{StatusCode, header};
use axum::{
    Json, Router,
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    response::IntoResponse,
    routing::{get, post},
};
use futures::{SinkExt, StreamExt};
use groundstation_shared::{BoardStatusMsg, FlightState, TelemetryCommand, TelemetryRow};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

// NEW

static FAVICON_DATA: OnceCell<Vec<u8>> = OnceCell::const_new();
static TILE_DB_POOL: OnceCell<Option<sqlx::SqlitePool>> = OnceCell::const_new();
static TILE_DB_MODE: OnceCell<TileDbMode> = OnceCell::const_new();

#[cfg(feature = "hitl_mode")]
fn hitl_actions() -> Vec<layout::ActionSpec> {
    let mk = |label: &str, cmd: &str| layout::ActionSpec {
        label: label.to_string(),
        cmd: cmd.to_string(),
        border: "#38bdf8".to_string(),
        bg: "#0b1220".to_string(),
        fg: "#e0f2fe".to_string(),
    };
    vec![
        mk("Deploy Parachute", "DeployParachute"),
        mk("Expand Parachute", "ExpandParachute"),
        mk("Reinit Sensors", "ReinitSensors"),
        mk("Launch Signal", "LaunchSignal"),
        mk("Evaluation Relax", "EvaluationRelax"),
        mk("Evaluation Focus", "EvaluationFocus"),
        mk("Evaluation Abort", "EvaluationAbort"),
        mk("Reinit Barometer", "ReinitBarometer"),
        mk("Enable IMU", "EnableIMU"),
        mk("Disable IMU", "DisableIMU"),
        mk("Monitor Altitude", "MonitorAltitude"),
        mk("Revoke Monitor Alt", "RevokeMonitorAltitude"),
        mk("Consecutive Samples", "ConsecutiveSamples"),
        mk("Revoke Consecutive", "RevokeConsecutiveSamples"),
        mk("Reset Failures", "ResetFailures"),
        mk("Revoke Reset Fail", "RevokeResetFailures"),
        mk("Validate Measms", "ValidateMeasms"),
        mk("Revoke Validate", "RevokeValidateMeasms"),
        mk("Abort After 15", "AbortAfter15"),
        mk("Abort After 40", "AbortAfter40"),
        mk("Abort After 70", "AbortAfter70"),
        mk("Reinit After 12", "ReinitAfter12"),
        mk("Reinit After 26", "ReinitAfter26"),
        mk("Reinit After 44", "ReinitAfter44"),
        mk("Flight State +1", "AdvanceFlightState"),
        mk("Flight State -1", "RewindFlightState"),
    ]
}

#[derive(Clone, Copy)]
enum TileDbMode {
    LegacyInline,
    Deduped,
}

fn values_from_row(row: &sqlx::sqlite::SqliteRow) -> Vec<Option<f32>> {
    let values_from_json = row
        .try_get::<Option<String>, _>("values_json")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str::<Vec<Option<f64>>>(&raw).ok());
    if let Some(values) = values_from_json
        && !values.is_empty()
    {
        return values.into_iter().map(|v| v.map(|n| n as f32)).collect();
    }

    if let Ok(raw) = row.try_get::<Option<String>, _>("payload_json")
        && let Some(raw) = raw
        && let Ok(bytes) = serde_json::from_str::<Vec<u8>>(&raw)
        && bytes.len().is_multiple_of(4)
        && !bytes.is_empty()
    {
        let mut out = Vec::with_capacity(bytes.len() / 4);
        for chunk in bytes.chunks_exact(4) {
            let arr = [chunk[0], chunk[1], chunk[2], chunk[3]];
            out.push(Some(f32::from_le_bytes(arr)));
        }
        return out;
    }

    Vec::new()
}

fn value_at(values: &[Option<f32>], idx: usize) -> Option<f32> {
    values.get(idx).copied().flatten()
}

/// Public router constructor
pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = ServeDir::new("./frontend/dist/public")
        .precompressed_br()
        .precompressed_gzip();
    Router::new()
        .layer(CompressionLayer::new())
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/alerts", get(get_alerts))
        .route("/api/boards", get(get_boards))
        .route("/api/layout", get(get_layout))
        .route("/api/calibration_config", get(get_calibration_config))
        .route("/api/map_config", get(get_map_config))
        .route(
            "/api/loadcell_calibration",
            get(get_loadcell_calibration).post(set_loadcell_calibration),
        )
        .route(
            "/api/loadcell_calibration/capture_zero",
            post(capture_loadcell_zero),
        )
        .route(
            "/api/loadcell_calibration/capture_span",
            post(capture_loadcell_span),
        )
        .route(
            "/api/loadcell_calibration/refit",
            post(refit_loadcell_channel),
        )
        .route(
            "/api/calibration",
            get(get_loadcell_calibration).post(set_loadcell_calibration),
        )
        .route("/api/calibration/capture_zero", post(capture_loadcell_zero))
        .route("/api/calibration/capture_span", post(capture_loadcell_span))
        .route("/api/calibration/refit", post(refit_loadcell_channel))
        .route("/api/network_time", get(get_network_time))
        .route("/api/notifications", get(get_notifications))
        .route(
            "/api/notifications/{id}/dismiss",
            post(dismiss_notification),
        )
        .route("/api/action_policy", get(get_action_policy))
        .route("/favicon", get(get_favicon))
        .route("/flightstate", get(get_flight_state))
        .route("/api/gps", get(get_gps))
        .route("/ws", get(ws_handler))
        .route("/tiles/{z}/{x}/{y}", get(get_tile_jpg))
        .route("/favicon.ico", get(get_favicon))
        .route("/valvestate", get(get_valve_state))
        // anything that doesn’t match the above routes goes to the static files
        .fallback_service(static_dir)
        .with_state(state)
}

/// Outgoing WebSocket messages to the frontend.
/// This is what the frontend will deserialize:
///   { "ty": "telemetry_batch", "data": [ ...TelemetryRow... ] }
///   { "ty": "warning",   "data": { ...WarningMsg... } }
///   { "ty": "error",     "data": { ...ErrorMsg... } }
#[derive(Serialize)]
#[serde(tag = "ty", content = "data")]
pub enum WsOutMsg {
    TelemetryBatch(Vec<TelemetryRow>),
    Warning(WarningMsg),
    FlightState(FlightStateMsg),
    Error(ErrorMsg),
    BoardStatus(BoardStatusMsg),
    Notifications(Vec<PersistentNotification>),
    ActionPolicy(ActionPolicyMsg),
    NetworkTime(NetworkTimeMsg),
}

#[derive(Clone, Serialize)]
pub struct FlightStateMsg {
    pub state: FlightState,
}

#[derive(Clone, Serialize)]
pub struct ValveStateMsg {
    pub timestamp_ms: i64,
    pub pilot_open: Option<bool>,
    pub normally_open_open: Option<bool>,
    pub dump_open: Option<bool>,
    pub igniter_on: Option<bool>,
    pub nitrogen_open: Option<bool>,
    pub nitrous_open: Option<bool>,
    pub retract_plumbing: Option<bool>,
}
/// Warning row sent to frontend (and stored in AppState channel)
#[derive(Clone, Serialize)]
pub struct WarningMsg {
    pub timestamp_ms: i64,
    pub message: String,
}

/// Error row sent to frontend (and stored in AppState channel)
#[derive(Clone, Serialize)]
pub struct ErrorMsg {
    pub timestamp_ms: i64,
    pub message: String,
}

/// DTO returned by /api/alerts
/// Frontend expects:
///   [{ "timestamp_ms": i64, "severity": "warning"|"error", "message": "..." }, ...]
#[derive(Serialize)]
pub struct AlertDto {
    pub timestamp_ms: i64,
    pub severity: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct GpsPoint {
    pub lat: f64,
    pub lon: f64,
}

#[derive(Serialize)]
pub struct GpsResponse {
    pub rocket: Option<GpsPoint>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NetworkTimeMsg {
    pub timestamp_ms: i64,
}

async fn get_valve_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Latest valve state row: data_type = 'VALVE_STATE'
    let row = sqlx::query(
        r#"
        SELECT timestamp_ms, values_json, payload_json
        FROM telemetry
        WHERE data_type = 'VALVE_STATE'
        ORDER BY timestamp_ms DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let valve_state = row.map(|r| {
        let timestamp_ms: i64 = r.get::<i64, _>("timestamp_ms");
        let values = values_from_row(&r);
        let to_bool = |idx: usize| -> Option<bool> { value_at(&values, idx).map(|x| x >= 0.5) };

        ValveStateMsg {
            timestamp_ms,
            pilot_open: to_bool(0),
            normally_open_open: to_bool(1),
            dump_open: to_bool(2),
            igniter_on: to_bool(3),
            nitrogen_open: to_bool(4),
            nitrous_open: to_bool(5),
            retract_plumbing: to_bool(6),
        }
    });

    Json(valve_state)
}

async fn get_gps(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Latest GPS row: assumes `data_type = 'GPS'`
    let row = sqlx::query(
        r#"
        SELECT values_json, payload_json
        FROM telemetry
        WHERE data_type = 'GPS'
        ORDER BY timestamp_ms DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let rocket = row.and_then(|r| {
        let values = values_from_row(&r);
        match (value_at(&values, 0), value_at(&values, 1)) {
            (Some(lat), Some(lon)) => Some(GpsPoint {
                lat: lat as f64,
                lon: lon as f64,
            }),
            _ => None,
        }
    });

    Json(GpsResponse { rocket })
}

async fn get_layout() -> impl IntoResponse {
    match layout::load_layout() {
        Ok(layout) => {
            #[allow(unused_mut)]
            let mut layout = layout;
            #[cfg(feature = "hitl_mode")]
            {
                for action in hitl_actions() {
                    if !layout
                        .actions_tab
                        .actions
                        .iter()
                        .any(|a| a.cmd == action.cmd)
                    {
                        layout.actions_tab.actions.push(action);
                    }
                }
            }
            Json(layout).into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err).into_response(),
    }
}

async fn get_calibration_config() -> impl IntoResponse {
    Json(loadcell::calibration_tab_layout()).into_response()
}

#[derive(Serialize)]
struct MapConfigDto {
    max_native_zoom: u32,
}

async fn get_map_config() -> impl IntoResponse {
    let max_native_zoom = match detect_max_native_zoom(DEFAULT_MAP_REGION).await {
        Ok(Some(z)) => z,
        Ok(None) => 12,
        Err(e) => {
            eprintln!("WARNING: failed to detect max native zoom: {e:#}");
            12
        }
    };

    Json(MapConfigDto { max_native_zoom })
}

#[derive(Deserialize)]
struct CaptureLoadcellPointReq {
    sensor_id: String,
    raw: f32,
}

#[derive(Deserialize)]
struct CaptureLoadcellSpanReq {
    sensor_id: String,
    raw: f32,
    known_kg: f32,
}

#[derive(Deserialize)]
struct RefitLoadcellReq {
    channel: String,
    mode: String,
}

async fn get_loadcell_calibration(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.loadcell_calibration.lock().unwrap().clone())
}

async fn set_loadcell_calibration(
    State(state): State<Arc<AppState>>,
    Json(cfg): Json<loadcell::LoadcellCalibrationFile>,
) -> impl IntoResponse {
    {
        let mut slot = state.loadcell_calibration.lock().unwrap();
        *slot = cfg.clone();
    }
    if let Err(err) = loadcell::save(&cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    Json(cfg).into_response()
}

async fn capture_loadcell_zero(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CaptureLoadcellPointReq>,
) -> impl IntoResponse {
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        loadcell::capture_zero(&mut cfg, &req.sensor_id, req.raw);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    Json(updated).into_response()
}

async fn capture_loadcell_span(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CaptureLoadcellSpanReq>,
) -> impl IntoResponse {
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        loadcell::capture_span(&mut cfg, &req.sensor_id, req.raw, req.known_kg);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    Json(updated).into_response()
}

async fn refit_loadcell_channel(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RefitLoadcellReq>,
) -> impl IntoResponse {
    let Some(channel) = loadcell::CalibrationChannel::from_str(req.channel.trim()) else {
        return (StatusCode::BAD_REQUEST, "invalid channel".to_string()).into_response();
    };
    let Some(mode) = loadcell::FitMode::from_str(req.mode.trim()) else {
        return (StatusCode::BAD_REQUEST, "invalid fit mode".to_string()).into_response();
    };

    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        if let Err(err) = loadcell::refit_channel(&mut cfg, channel, mode) {
            return (StatusCode::BAD_REQUEST, err).into_response();
        }
        cfg.clone()
    };

    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    Json(updated).into_response()
}

async fn get_tile_jpg(Path((z, x, y_raw)): Path<(u32, u32, String)>) -> impl IntoResponse {
    let y_trimmed = y_raw.trim_end_matches(".jpg");
    let y: u32 = match y_trimmed.parse() {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if let Some(bytes) = read_tile_with_fallback(z, x, y).await {
        return ([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

fn tile_path(z: u32, x: u32, y: u32) -> PathBuf {
    format!("./backend/data/maps/{DEFAULT_MAP_REGION}/tiles/{z}/{x}/{y}.jpg").into()
}

async fn tile_db_pool() -> Option<sqlx::SqlitePool> {
    TILE_DB_POOL
        .get_or_init(|| async {
            let path = tile_bundle_path(DEFAULT_MAP_REGION);
            let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
            if !exists {
                return None;
            }
            let url = format!("sqlite://{}?mode=ro", path.to_string_lossy());
            match SqlitePoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
            {
                Ok(pool) => Some(pool),
                Err(e) => {
                    eprintln!(
                        "WARNING: failed to open tile bundle {}: {e}",
                        path.display()
                    );
                    None
                }
            }
        })
        .await
        .clone()
}

async fn tile_db_mode() -> TileDbMode {
    *TILE_DB_MODE
        .get_or_init(|| async {
            let Some(pool) = tile_db_pool().await else {
                return TileDbMode::LegacyInline;
            };
            let Ok(rows) = sqlx::query("PRAGMA table_info(tiles)")
                .fetch_all(&pool)
                .await
            else {
                return TileDbMode::LegacyInline;
            };
            let has_blob_id = rows.iter().any(|r| {
                r.try_get::<String, _>("name")
                    .map(|n| n == "blob_id")
                    .unwrap_or(false)
            });
            if has_blob_id {
                TileDbMode::Deduped
            } else {
                TileDbMode::LegacyInline
            }
        })
        .await
}

async fn read_exact_tile(z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
    if let Some(pool) = tile_db_pool().await {
        match tile_db_mode().await {
            TileDbMode::LegacyInline => {
                match sqlx::query_scalar::<_, Vec<u8>>(
                    "SELECT image FROM tiles WHERE z = ? AND x = ? AND y = ? LIMIT 1",
                )
                .bind(i64::from(z))
                .bind(i64::from(x))
                .bind(i64::from(y))
                .fetch_optional(&pool)
                .await
                {
                    Ok(Some(bytes)) => return Some(bytes),
                    Ok(None) => {}
                    Err(e) => eprintln!("WARNING: failed reading tile from bundle: {e}"),
                }
            }
            TileDbMode::Deduped => {
                match sqlx::query_scalar::<_, Vec<u8>>(
                    "SELECT b.image
                     FROM tiles t
                     JOIN tile_blobs b ON b.id = t.blob_id
                     WHERE t.z = ? AND t.x = ? AND t.y = ?
                     LIMIT 1",
                )
                .bind(i64::from(z))
                .bind(i64::from(x))
                .bind(i64::from(y))
                .fetch_optional(&pool)
                .await
                {
                    Ok(Some(bytes)) => return Some(bytes),
                    Ok(None) => {}
                    Err(e) => eprintln!("WARNING: failed reading deduped tile from bundle: {e}"),
                }
            }
        }
    }

    let exact_path = tile_path(z, x, y);
    match tokio::fs::read(&exact_path).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() != ErrorKind::NotFound => {
            eprintln!("WARNING: failed reading tile {}: {e}", exact_path.display());
            None
        }
        Err(_) => None,
    }
}

async fn read_tile_with_fallback(z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
    if let Some(bytes) = read_exact_tile(z, x, y).await {
        return Some(bytes);
    }

    let mut az = z;
    let mut ax = x;
    let mut ay = y;
    while az > 0 {
        az -= 1;
        ax /= 2;
        ay /= 2;
        let Some(parent_bytes) = read_exact_tile(az, ax, ay).await else {
            continue;
        };
        match synthesize_zoom_tile_from_ancestor(&parent_bytes, az, ax, ay, z, x, y) {
            Ok(bytes) => return Some(bytes),
            Err(err) => {
                eprintln!(
                    "WARNING: failed synthesizing fallback tile z={z} x={x} y={y} from ancestor z={az} x={ax} y={ay}: {err}"
                );
            }
        }
    }
    None
}

fn synthesize_zoom_tile_from_ancestor(
    ancestor_jpg: &[u8],
    ancestor_z: u32,
    ancestor_x: u32,
    ancestor_y: u32,
    target_z: u32,
    target_x: u32,
    target_y: u32,
) -> Result<Vec<u8>, String> {
    if target_z < ancestor_z {
        return Err("target zoom is lower than ancestor zoom".to_string());
    }
    let depth = target_z - ancestor_z;
    if depth == 0 {
        return Ok(ancestor_jpg.to_vec());
    }

    let rel_x = target_x.saturating_sub(ancestor_x << depth);
    let rel_y = target_y.saturating_sub(ancestor_y << depth);

    let mut img = image::load_from_memory(ancestor_jpg).map_err(|e| e.to_string())?;
    for bit in (0..depth).rev() {
        let qx = (rel_x >> bit) & 1;
        let qy = (rel_y >> bit) & 1;
        let (w, h) = img.dimensions();
        if w < 2 || h < 2 {
            break;
        }
        let cw = w / 2;
        let ch = h / 2;
        let ox = if qx == 1 { cw } else { 0 };
        let oy = if qy == 1 { ch } else { 0 };

        let cropped = img.crop_imm(ox, oy, cw.max(1), ch.max(1)).to_rgb8();
        let resized = image::imageops::resize(&cropped, 256, 256, FilterType::CatmullRom);
        img = DynamicImage::ImageRgb8(resized);
    }

    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    let mut enc = JpegEncoder::new_with_quality(&mut out, 85);
    enc.encode(
        &rgb,
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| e.to_string())?;
    Ok(out)
}

async fn get_recent(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let latest_db_ts: Option<i64> = sqlx::query_scalar("SELECT MAX(timestamp_ms) FROM telemetry")
        .fetch_one(&state.db)
        .await
        .ok()
        .flatten();
    let cache_snapshot = state.recent_telemetry_snapshot();
    let latest_cache_ts = cache_snapshot.iter().map(|r| r.timestamp_ms).max();

    let now_ms = match (latest_db_ts, latest_cache_ts) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return Json(Vec::<TelemetryRow>::new()),
    };

    let cutoff = now_ms - 20 * 60 * 1000; // 20 minutes

    let rows_db = sqlx::query(
        r#"
        SELECT
            timestamp_ms,
            data_type,
            values_json,
            payload_json
        FROM telemetry
        WHERE timestamp_ms BETWEEN ? AND ?
        ORDER BY timestamp_ms ASC
        "#,
    )
    .bind(cutoff)
    .bind(now_ms)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut by_key: HashMap<(String, i64), TelemetryRow> = HashMap::new();
    for row in rows_db {
        let item = TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            values: values_from_row(&row),
        };
        by_key.insert((item.data_type.clone(), item.timestamp_ms), item);
    }

    for row in cache_snapshot {
        if row.timestamp_ms < cutoff || row.timestamp_ms > now_ms {
            continue;
        }
        // Cache rows are newest realtime view and should win over stale DB rows.
        by_key.insert((row.data_type.clone(), row.timestamp_ms), row);
    }

    let mut rows: Vec<TelemetryRow> = by_key.into_values().collect();
    rows.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.data_type.cmp(&b.data_type))
    });

    Json(rows)
}

async fn get_favicon() -> impl IntoResponse {
    // Load the favicon into memory on first request, reuse later
    let bytes = FAVICON_DATA
        .get_or_init(|| async {
            // Adjust this path if needed
            let path: PathBuf = "./frontend/assets/icon.png".into();
            tokio::fs::read(&path)
                .await
                .unwrap_or_else(|e| panic!("failed to read favicon at {:?}: {e}", path))
        })
        .await
        .clone();

    // Return as an image/png response
    ([(header::CONTENT_TYPE, "image/png")], bytes)
}
async fn get_flight_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // get the state from the db
    let flight_state: i64 =
        match sqlx::query("SELECT f_state FROM flight_state ORDER BY timestamp_ms DESC LIMIT 1")
            .fetch_one(&state.db)
            .await
        {
            Ok(data) => data.get::<i64, _>("f_state"),
            Err(_) => FlightState::Startup as i64,
        };
    let flight_state = groundstation_shared::u8_to_flight_state(flight_state as u8)
        .unwrap_or(FlightState::Startup);
    Json(flight_state)
}

async fn get_network_time() -> impl IntoResponse {
    Json(NetworkTimeMsg {
        timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
    })
}

async fn get_notifications(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.notifications_snapshot())
}

async fn dismiss_notification(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> impl IntoResponse {
    if state.dismiss_notification(id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn get_action_policy(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(state.action_policy_snapshot())
}
async fn send_command(
    State(state): State<Arc<AppState>>,
    Json(cmd): Json<TelemetryCommand>,
) -> &'static str {
    let _ = state.cmd_tx.send(cmd).await;
    "ok"
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Shape of commands sent from the frontend over WebSocket:
/// { "cmd": "Arm" } or { "cmd": "Disarm" }
#[derive(Deserialize)]
struct WsCommand {
    cmd: TelemetryCommand,
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>) {
    // Subscribe to all three broadcast channels
    let mut telemetry_rx = state.ws_tx.subscribe();
    let mut warnings_rx = state.warnings_tx.subscribe();
    let mut errors_rx = state.errors_tx.subscribe();
    let mut state_rx = state.state_tx.subscribe();
    let mut board_status_rx = state.board_status_tx.subscribe();
    let mut notifications_rx = state.notifications_tx.subscribe();
    let mut action_policy_rx = state.action_policy_tx.subscribe();

    let cmd_tx = state.cmd_tx.clone();
    let (mut socket_sender, mut receiver) = socket.split();
    let ws_out_queue_cap: usize = std::env::var("GS_WS_OUT_QUEUE_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(512)
        .clamp(32, 8192);
    let (ws_out_tx, mut ws_out_rx) = mpsc::channel::<String>(ws_out_queue_cap);

    // Dedicated writer: continuously drains queued outbound messages.
    let write_task = tokio::spawn(async move {
        while let Some(text) = ws_out_rx.recv().await {
            if socket_sender
                .send(Message::Text(Utf8Bytes::from(text)))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Task: collect, batch, and enqueue outbound messages.
    let send_task = async move {
        let initial_notifications =
            serde_json::to_string(&WsOutMsg::Notifications(state.notifications_snapshot()))
                .unwrap_or_default();
        if ws_out_tx.send(initial_notifications).await.is_err() {
            return;
        }
        let initial_action_policy =
            serde_json::to_string(&WsOutMsg::ActionPolicy(state.action_policy_snapshot()))
                .unwrap_or_default();
        if ws_out_tx.send(initial_action_policy).await.is_err() {
            return;
        }
        let initial_network_time = serde_json::to_string(&WsOutMsg::NetworkTime(NetworkTimeMsg {
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
        }))
        .unwrap_or_default();
        if ws_out_tx.send(initial_network_time).await.is_err() {
            return;
        }

        let adaptive_rate = std::env::var("GS_WS_ADAPTIVE_RATE").ok().as_deref() != Some("0");
        let mut network_time_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let telemetry_flush_ms: u64 = std::env::var("GS_WS_TELEMETRY_FLUSH_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(20)
            .clamp(5, 1000);
        let max_telemetry_per_flush_cap: usize = std::env::var("GS_WS_MAX_TELEMETRY_PER_FLUSH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(256)
            .clamp(8, 2048);
        let min_telemetry_per_flush: usize = std::env::var("GS_WS_MIN_TELEMETRY_PER_FLUSH")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(64)
            .clamp(1, max_telemetry_per_flush_cap);
        let telemetry_pending_cap: usize = std::env::var("GS_WS_PENDING_CAP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(16_384)
            .clamp(256, 262_144);
        let mut dynamic_max_per_flush = ((max_telemetry_per_flush_cap * 3) / 4)
            .clamp(min_telemetry_per_flush, max_telemetry_per_flush_cap);
        let mut telemetry_pending: VecDeque<TelemetryRow> = VecDeque::new();
        let mut telemetry_flush =
            tokio::time::interval(std::time::Duration::from_millis(telemetry_flush_ms));

        loop {
            tokio::select! {
                biased;

                recv = warnings_rx.recv() => {
                    match recv {
                        Ok(warn) => {
                            let msg = WsOutMsg::Warning(warn);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = errors_rx.recv() => {
                    match recv {
                        Ok(err) => {
                            let msg = WsOutMsg::Error(err);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = state_rx.recv() => {
                    match recv {
                        Ok(fs) => {
                            let msg = WsOutMsg::FlightState(fs);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = board_status_rx.recv() => {
                    match recv {
                        Ok(status) => {
                            let msg = WsOutMsg::BoardStatus(status);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = notifications_rx.recv() => {
                    match recv {
                        Ok(snapshot) => {
                            let msg = WsOutMsg::Notifications(snapshot);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = action_policy_rx.recv() => {
                    match recv {
                        Ok(policy) => {
                            let msg = WsOutMsg::ActionPolicy(policy);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                _ = network_time_tick.tick() => {
                    let msg = WsOutMsg::NetworkTime(NetworkTimeMsg {
                        timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                    });
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    if ws_out_tx.send(text).await.is_err() {
                        break;
                    }
                }

                recv = telemetry_rx.recv() => {
                    match recv {
                        Ok(pkt) => {
                            telemetry_pending.push_back(pkt);
                            while telemetry_pending.len() > telemetry_pending_cap {
                                telemetry_pending.pop_front();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // On lag, keep going and flush what we already have.
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                _ = telemetry_flush.tick() => {
                    if telemetry_pending.is_empty() {
                        continue;
                    }

                    let pending_before = telemetry_pending.len();

                    let max_this_flush = if adaptive_rate {
                        dynamic_max_per_flush
                    } else {
                        max_telemetry_per_flush_cap
                    };
                    if telemetry_pending.len() > max_this_flush {
                        let drop_n = telemetry_pending.len() - max_this_flush;
                        telemetry_pending.drain(0..drop_n);
                    }
                    let mut rows: Vec<TelemetryRow> = telemetry_pending.drain(..).collect();
                    rows.sort_by_key(|r| r.timestamp_ms);

                    let msg = WsOutMsg::TelemetryBatch(rows);
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    let queue_was_full = match ws_out_tx.try_send(text) {
                        Ok(()) => false,
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    };

                    if adaptive_rate {
                        let congested = queue_was_full && pending_before >= max_this_flush;
                        if congested {
                            dynamic_max_per_flush = (dynamic_max_per_flush / 2).max(min_telemetry_per_flush);
                        } else if pending_before >= max_this_flush.saturating_sub(1)
                        {
                            dynamic_max_per_flush = (dynamic_max_per_flush + 8).min(max_telemetry_per_flush_cap);
                        }
                    }
                }
            }
        }
    };

    // Task: client -> server (commands)
    let recv_task = async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<WsCommand>(&text) {
                    Ok(cmd) => {
                        if let Err(e) = cmd_tx.send(cmd.cmd).await {
                            println!("Failed to forward WS command to cmd_tx: {e}");
                        }
                    }
                    Err(e) => {
                        println!("Invalid WS command JSON {text:?}: {e}");
                    }
                }
            }
        }
    };

    // Run both directions until one side ends
    tokio::join!(send_task, recv_task);
    write_task.abort();
}

#[derive(Deserialize)]
struct HistoryParams {
    // /api/history?minutes=20  (defaults to 20 if not provided)
    minutes: Option<u64>,
}

/// NEW: /api/alerts – returns warnings + errors from `alerts` table
/// Query param: `minutes` (optional, defaults to 20)
async fn get_alerts(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HistoryParams>,
) -> impl IntoResponse {
    let minutes = params.minutes.unwrap_or(20);
    let cutoff = now_ms_i64() - (minutes as i64) * 60_000;

    let alerts_db = sqlx::query(
        r#"
        SELECT timestamp_ms, severity, message
        FROM alerts
        WHERE timestamp_ms >= ?
        ORDER BY timestamp_ms DESC
        "#,
    )
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let alerts: Vec<AlertDto> = alerts_db
        .into_iter()
        .map(|row| AlertDto {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            severity: row.get::<String, _>("severity"),
            message: row.get::<String, _>("message"),
        })
        .collect();

    Json(alerts)
}

async fn get_boards(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let now_ms = now_ms_i64().max(0) as u64;
    let msg = state.board_status_snapshot(now_ms);
    Json(msg)
}

/// Helper: current timestamp in ms (i64) for warnings/errors/etc.
fn now_ms_i64() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

fn spawn_alert_insert(
    state: &AppState,
    timestamp_ms: i64,
    severity: &'static str,
    message: String,
) {
    state.begin_db_write();
    let db = state.db.clone();
    let state_for_task = state.clone();

    tokio::spawn(async move {
        let _ = sqlx::query(
            r#"
            INSERT INTO alerts (timestamp_ms, severity, message)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(timestamp_ms)
        .bind(severity)
        .bind(message)
        .execute(&db)
        .await;
        state_for_task.end_db_write();
    });
}

/// PUBLIC HELPERS — can be called from *any thread* that has &AppState
///
/// Example usage from anywhere:
///     emit_warning(&app_state, "GPS fix lost");
///     emit_error(&app_state, "Main valve stuck closed");
///
/// If you only have Arc<AppState>, just pass `&*arc` or `arc.as_ref()`.
pub fn emit_warning<S: Into<String>>(state: &AppState, message: S) {
    let msg_string = message.into();
    let timestamp = now_ms_i64();

    // 1) Broadcast to frontend immediately
    let ws_msg = WarningMsg {
        timestamp_ms: timestamp,
        message: msg_string.clone(),
    };
    let _ = state.warnings_tx.send(ws_msg);

    // 2) Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "warning", msg_string);
}

/// Log a warning to the DB without sending it to the frontend.
pub fn emit_warning_db_only<S: Into<String>>(state: &AppState, message: S) {
    let msg_string = message.into();
    let timestamp = now_ms_i64();

    // Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "warning", msg_string);
}

pub fn emit_error<S: Into<String>>(state: &AppState, message: S) {
    let msg_string = message.into();
    let timestamp = now_ms_i64();

    // 1) Broadcast to frontend immediately
    let ws_msg = ErrorMsg {
        timestamp_ms: timestamp,
        message: msg_string.clone(),
    };
    let _ = state.errors_tx.send(ws_msg);

    // 2) Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "error", msg_string);
}
