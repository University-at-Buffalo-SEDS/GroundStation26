use crate::auth::{AuthFailure, LoginRequest, Permission};
use crate::fill_targets::{self, FillTargetsConfig};
use crate::flight_setup::{self, FlightSetupConfig};
use crate::i18n::{self, TranslateRequest, TranslateResponse, TranslationCatalogResponse};
use crate::layout;
use crate::loadcell;
use crate::map::{DEFAULT_MAP_REGION, detect_max_native_zoom, tile_bundle_path};
use crate::sequences::{ActionPolicyMsg, PersistentNotification, command_name};
use crate::state::AppState;
use crate::telemetry_db::{DbQueueItem, DbWrite, LaunchClockMsg, RecordingStatusMsg};
use crate::types::{
    BoardStatusMsg, FlightState, NetworkTopologyMsg, TelemetryCommand, TelemetryRow,
};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::{
    Json, Router,
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::{get, get_service, post},
};
use futures::{SinkExt, StreamExt, TryStreamExt, stream};
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqlitePoolOptions;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{OnceCell, mpsc};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

// NEW

static FAVICON_DATA: OnceCell<Option<Vec<u8>>> = OnceCell::const_new();
static TILE_DB_POOL: OnceCell<Option<sqlx::SqlitePool>> = OnceCell::const_new();
static TILE_DB_MODE: OnceCell<TileDbMode> = OnceCell::const_new();
const RECENT_HISTORY_MS: i64 = 20 * 60 * 1000;
const RECENT_BUCKET_MS: i64 = 20;
const SOFTWARE_COMMAND_DEDUP_MS_DEFAULT: u64 = 500;

#[derive(Clone, Copy)]
enum TileDbMode {
    LegacyInline,
    Deduped,
}

#[derive(Deserialize)]
struct TranslationCatalogQuery {
    #[serde(default = "default_translation_lang")]
    lang: String,
}

fn default_translation_lang() -> String {
    "en".to_string()
}

fn software_command_dedup_ms() -> u64 {
    std::env::var("GS_SOFTWARE_COMMAND_DEDUP_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(SOFTWARE_COMMAND_DEDUP_MS_DEFAULT)
}

/// Extracts the numeric telemetry payload from either the newer JSON column or the legacy blob.
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

/// Returns the flattened channel value at `idx` when present.
fn value_at(values: &[Option<f32>], idx: usize) -> Option<f32> {
    values.get(idx).copied().flatten()
}

/// Buckets recent telemetry so the reseed endpoint returns a bounded UI-friendly payload.
fn compact_recent_rows(rows: Vec<TelemetryRow>, cutoff: i64) -> Vec<TelemetryRow> {
    let mut by_bucket: HashMap<(String, String, i64), TelemetryRow> = HashMap::new();
    for row in rows {
        if row.timestamp_ms < cutoff {
            continue;
        }
        // Keep the newest row for each sender/data-type bucket so reconnect reseeds stay compact.
        let bucket = row.timestamp_ms.div_euclid(RECENT_BUCKET_MS);
        by_bucket.insert((row.data_type.clone(), row.sender_id.clone(), bucket), row);
    }

    let mut rows: Vec<TelemetryRow> = by_bucket.into_values().collect();
    rows.sort_by(|a, b| {
        a.timestamp_ms
            .cmp(&b.timestamp_ms)
            .then_with(|| a.sender_id.cmp(&b.sender_id))
            .then_with(|| a.data_type.cmp(&b.data_type))
    });
    rows
}

fn wants_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|mime| mime.starts_with("application/x-ndjson"))
        })
        .unwrap_or(false)
}

/// Public router constructor
pub fn router(state: Arc<AppState>) -> Router {
    let spa_index = ServeFile::new("./frontend/dist/public/index.html");
    let static_dir = ServeDir::new("./frontend/dist/public")
        .precompressed_br()
        .precompressed_gzip()
        .not_found_service(spa_index.clone());
    Router::new()
        .layer(CompressionLayer::new())
        .route("/", get_service(spa_index.clone()))
        .route("/connect", get_service(spa_index.clone()))
        .route("/login", get_service(spa_index.clone()))
        .route("/dashboard", get_service(spa_index))
        .route(
            "/version",
            get_service(ServeFile::new("./frontend/dist/public/index.html")),
        )
        .route("/api/auth/login", post(login))
        .route("/api/auth/session", get(get_session_status))
        .route("/api/auth/logout", post(logout))
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/alerts", get(get_alerts))
        .route("/api/boards", get(get_boards))
        .route("/api/layout", get(get_layout))
        .route("/api/i18n/catalog", get(get_translation_catalog))
        .route("/api/i18n/translate", post(post_translate_texts))
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
        .route("/api/launch_clock", get(get_launch_clock))
        .route("/api/network_topology", get(get_network_topology))
        .route("/api/recording_status", get(get_recording_status))
        .route("/api/notifications", get(get_notifications))
        .route(
            "/api/notifications/{id}/dismiss",
            post(dismiss_notification),
        )
        .route("/api/action_policy", get(get_action_policy))
        .route(
            "/api/flight_setup",
            get(get_flight_setup).post(set_flight_setup),
        )
        .route("/api/flight_setup/apply", post(apply_flight_setup))
        .route(
            "/api/fill_targets",
            get(get_fill_targets).post(set_fill_targets),
        )
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

/// Pulls a bearer token out of the Authorization header.
fn bearer_token_from_headers(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Converts auth-domain failures into the matching HTTP response.
fn auth_failure_response(err: AuthFailure) -> axum::response::Response {
    match err {
        AuthFailure::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg).into_response(),
        AuthFailure::Forbidden(msg) => (StatusCode::FORBIDDEN, msg).into_response(),
        AuthFailure::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response(),
    }
}

/// Authorizes an HTTP request against the required permission set.
async fn authorize_headers(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    required: Permission,
) -> Result<crate::auth::AuthPrincipal, axum::response::Response> {
    state
        .auth
        .authorize_token(&state.auth_db, bearer_token_from_headers(headers), required)
        .await
        .map_err(auth_failure_response)
}

/// Creates a new authenticated session from username/password credentials.
async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginRequest>,
) -> impl IntoResponse {
    match state.auth.login(&state.auth_db, req).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => auth_failure_response(err),
    }
}

/// Returns whether the current request is associated with a valid session.
async fn get_session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    match state
        .auth
        .session_status(&state.auth_db, bearer_token_from_headers(&headers))
        .await
    {
        Ok(status) => Json(status).into_response(),
        Err(err) => auth_failure_response(err),
    }
}

/// Revokes the current session token when one is present.
async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(token) = bearer_token_from_headers(&headers) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match state.auth.logout(&state.auth_db, token).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => auth_failure_response(err),
    }
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
    LaunchClock(LaunchClockMsg),
    Error(ErrorMsg),
    BoardStatus(BoardStatusMsg),
    NetworkTopology(NetworkTopologyMsg),
    Notifications(Vec<PersistentNotification>),
    ActionPolicy(ActionPolicyMsg),
    FillTargets(FillTargetsConfig),
    RecordingStatus(RecordingStatusMsg),
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
#[derive(Clone, Serialize)]
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

/// Returns the most recent decoded valve-state row.
async fn get_valve_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    // Latest valve state row: data_type = 'VALVE_STATE'
    let db = state.telemetry_db_pool();
    let row = sqlx::query(
        r#"
        SELECT timestamp_ms, values_json, payload_json
        FROM telemetry
        WHERE data_type = 'VALVE_STATE'
        ORDER BY timestamp_ms DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(&db)
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

    Json(valve_state).into_response()
}

/// Returns the latest GPS fix stored in telemetry history.
async fn get_gps(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let db = state.telemetry_db_pool();
    // Prefer the normalized GPS stream, but keep compatibility with older row tags.
    let row = sqlx::query(
        r#"
        SELECT values_json, payload_json
        FROM telemetry
        WHERE data_type IN ('GPS_DATA', 'GPS', 'ROCKET_GPS')
        ORDER BY timestamp_ms DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(&db)
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

    Json(GpsResponse { rocket }).into_response()
}

/// Serves the current frontend layout configuration.
async fn get_layout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    match layout::load_layout() {
        Ok(layout) => Json(layout).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err).into_response(),
    }
}

async fn get_translation_catalog(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TranslationCatalogQuery>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json::<TranslationCatalogResponse>(i18n::catalog_for_lang(&query.lang)).into_response()
}

async fn post_translate_texts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<TranslateRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json::<TranslateResponse>(i18n::translate_texts(req).await).into_response()
}

/// Returns the UI layout metadata for the loadcell calibration tab.
async fn get_calibration_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(loadcell::calibration_tab_layout()).into_response()
}

#[derive(Serialize)]
struct MapConfigDto {
    max_native_zoom: u32,
    default_center_lat: f64,
    default_center_lon: f64,
    default_zoom: f64,
    map_title: String,
    tracked_asset_label: String,
}

/// Returns the map configuration derived from bundled map assets.
async fn get_map_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let max_native_zoom = match detect_max_native_zoom(DEFAULT_MAP_REGION).await {
        Ok(Some(z)) => z,
        Ok(None) => 12,
        Err(e) => {
            eprintln!("WARNING: failed to detect max native zoom: {e:#}");
            12
        }
    };

    Json(MapConfigDto {
        max_native_zoom,
        default_center_lat: 31.0,
        default_center_lon: -99.0,
        default_zoom: 7.0,
        map_title: "Map".to_string(),
        tracked_asset_label: "Tracked Asset".to_string(),
    })
    .into_response()
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

/// Returns the in-memory loadcell calibration file.
async fn get_loadcell_calibration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.loadcell_calibration.lock().unwrap().clone()).into_response()
}

/// Replaces the loadcell calibration file in memory and on disk.
async fn set_loadcell_calibration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cfg): Json<loadcell::LoadcellCalibrationFile>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    {
        let mut slot = state.loadcell_calibration.lock().unwrap();
        *slot = cfg.clone();
    }
    if let Err(err) = loadcell::save(&cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    state.broadcast_fill_targets_snapshot();
    Json(cfg).into_response()
}

/// Captures a zero point for the selected loadcell sensor.
async fn capture_loadcell_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CaptureLoadcellPointReq>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        loadcell::capture_zero(&mut cfg, &req.sensor_id, req.raw);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    state.broadcast_fill_targets_snapshot();
    Json(updated).into_response()
}

/// Captures a known-mass span point for the selected loadcell sensor.
async fn capture_loadcell_span(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CaptureLoadcellSpanReq>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        loadcell::capture_span(&mut cfg, &req.sensor_id, req.raw, req.known_kg);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    state.broadcast_fill_targets_snapshot();
    Json(updated).into_response()
}

/// Recomputes the selected calibration channel using the requested fit mode.
async fn refit_loadcell_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RefitLoadcellReq>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    let channel = loadcell::CalibrationChannel::from_str(req.channel.trim());
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
    state.broadcast_fill_targets_snapshot();
    Json(updated).into_response()
}

/// Serves a map tile or a synthesized ancestor fallback when the exact tile is missing.
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

/// Builds the legacy on-disk tile path for a map tile.
fn tile_path(z: u32, x: u32, y: u32) -> PathBuf {
    format!("./backend/data/maps/{DEFAULT_MAP_REGION}/tiles/{z}/{x}/{y}.jpg").into()
}

/// Lazily opens the read-only SQLite tile bundle when one exists.
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

/// Detects whether the tile bundle uses the legacy inline-image schema or the deduped blob schema.
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

/// Loads the exact tile from the bundle first and falls back to the legacy filesystem layout.
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

/// Walks up the tile pyramid until it can synthesize a usable child tile.
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

/// Crops and rescales an ancestor tile to approximate a missing descendant tile.
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

/// Returns the recent telemetry window, preferring the in-memory cache to cover DB write lag.
async fn get_recent(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    if wants_ndjson(&headers) {
        return stream_recent_rows_response(state).await;
    }

    let cache_snapshot = state.recent_telemetry_snapshot();
    let latest_cache_ts = cache_snapshot.last().map(|r| r.timestamp_ms);
    let oldest_cache_ts = cache_snapshot.first().map(|r| r.timestamp_ms);

    if let Some(now_ms) = latest_cache_ts {
        let cutoff = now_ms.saturating_sub(RECENT_HISTORY_MS);
        let cache_covers_window =
            oldest_cache_ts.is_some_and(|oldest| oldest <= cutoff + RECENT_BUCKET_MS);
        if cache_covers_window {
            return Json(compact_recent_rows(cache_snapshot, cutoff)).into_response();
        }

        let db_end_ms = oldest_cache_ts.unwrap_or(now_ms).saturating_sub(1);
        // Merge the persisted prefix with the live cache so reconnects can see just-written data.
        let mut merged_rows = if db_end_ms >= cutoff {
            load_recent_rows_from_db(&state, cutoff, db_end_ms).await
        } else {
            Vec::new()
        };
        merged_rows.extend(
            cache_snapshot
                .into_iter()
                .filter(|row| row.timestamp_ms >= cutoff && row.timestamp_ms <= now_ms),
        );
        return Json(compact_recent_rows(merged_rows, cutoff)).into_response();
    }

    let db = state.telemetry_db_pool();
    let Some(now_ms) =
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(timestamp_ms) FROM telemetry")
            .fetch_one(&db)
            .await
            .ok()
            .flatten()
    else {
        return Json(Vec::<TelemetryRow>::new()).into_response();
    };
    let cutoff = now_ms.saturating_sub(RECENT_HISTORY_MS);
    Json(compact_recent_rows(
        load_recent_rows_from_db(&state, cutoff, now_ms).await,
        cutoff,
    ))
    .into_response()
}

async fn stream_recent_rows_response(state: Arc<AppState>) -> Response {
    let cache_snapshot = state.recent_telemetry_snapshot();
    let latest_cache_ts = cache_snapshot.last().map(|r| r.timestamp_ms);
    let oldest_cache_ts = cache_snapshot.first().map(|r| r.timestamp_ms);

    let now_ms = if let Some(now_ms) = latest_cache_ts {
        now_ms
    } else {
        let db = state.telemetry_db_pool();
        if let Some(now_ms) =
            sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(timestamp_ms) FROM telemetry")
                .fetch_one(&db)
                .await
                .ok()
                .flatten()
        {
            now_ms
        } else {
            return (
                [(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")],
                Body::empty(),
            )
                .into_response();
        }
    };

    let cutoff = now_ms.saturating_sub(RECENT_HISTORY_MS);
    let cache_covers_window = latest_cache_ts.is_some()
        && oldest_cache_ts.is_some_and(|oldest| oldest <= cutoff + RECENT_BUCKET_MS);
    let db_end_ms = if cache_covers_window {
        None
    } else {
        oldest_cache_ts
            .unwrap_or(now_ms)
            .checked_sub(1)
            .filter(|end_ms| *end_ms >= cutoff)
    };

    let state_for_task = Arc::clone(&state);
    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        if let Some(end_ms) = db_end_ms {
            let db = state_for_task.telemetry_db_pool();
            let mut rows = sqlx::query(
                r#"
                SELECT
                    timestamp_ms,
                    data_type,
                    sender_id,
                    values_json,
                    payload_json
                FROM telemetry
                WHERE timestamp_ms BETWEEN ? AND ?
                ORDER BY timestamp_ms ASC
                "#,
            )
            .bind(cutoff)
            .bind(end_ms)
            .fetch(&db);

            while let Ok(Some(row)) = rows.try_next().await {
                let telemetry_row = TelemetryRow {
                    timestamp_ms: row.get::<i64, _>("timestamp_ms"),
                    data_type: row.get::<String, _>("data_type"),
                    sender_id: row.get::<String, _>("sender_id"),
                    values: values_from_row(&row),
                };
                if send_ndjson_row(&tx, &telemetry_row).await.is_err() {
                    return;
                }
            }
        }

        for row in cache_snapshot {
            if row.timestamp_ms < cutoff || row.timestamp_ms > now_ms {
                continue;
            }
            if send_ndjson_row(&tx, &row).await.is_err() {
                return;
            }
        }
    });

    let body_stream = stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<Vec<u8>, Infallible>(chunk), rx))
    });

    (
        [(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")],
        Body::from_stream(body_stream),
    )
        .into_response()
}

async fn send_ndjson_row(tx: &mpsc::Sender<Vec<u8>>, row: &TelemetryRow) -> Result<(), ()> {
    let Ok(mut bytes) = serde_json::to_vec(row) else {
        return Ok(());
    };
    bytes.push(b'\n');
    tx.send(bytes).await.map_err(|_| ())
}

/// Loads telemetry rows for a bounded time range directly from SQLite.
async fn load_recent_rows_from_db(
    state: &Arc<AppState>,
    start_ms: i64,
    end_ms: i64,
) -> Vec<TelemetryRow> {
    if end_ms < start_ms {
        return Vec::new();
    }

    let rows_db = sqlx::query(
        r#"
        SELECT
            timestamp_ms,
            data_type,
            sender_id,
            values_json,
            payload_json
        FROM telemetry
        WHERE timestamp_ms BETWEEN ? AND ?
        ORDER BY timestamp_ms ASC
        "#,
    )
    .bind(start_ms)
    .bind(end_ms)
    .fetch_all(&state.telemetry_db_pool())
    .await
    .unwrap_or_default();

    let mut out = Vec::with_capacity(rows_db.len());
    for row in rows_db {
        out.push(TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            sender_id: row.get::<String, _>("sender_id"),
            values: values_from_row(&row),
        });
    }
    out
}

/// Serves the app icon and caches it after the first disk read.
async fn get_favicon() -> impl IntoResponse {
    let bytes = FAVICON_DATA
        .get_or_init(|| async {
            let candidates: [PathBuf; 3] = [
                "./frontend/dist/public/icon.png".into(),
                "./frontend/dist/public/assets/icon.png".into(),
                "./frontend/dist/public/favicon.png".into(),
            ];
            for path in candidates {
                match tokio::fs::read(&path).await {
                    Ok(bytes) => return Some(bytes),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        eprintln!("failed to read favicon at {:?}: {err}", path);
                        return None;
                    }
                }
            }
            None
        })
        .await
        .clone();

    match bytes {
        Some(bytes) => {
            (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
/// Returns the latest persisted flight-state enum value.
async fn get_flight_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    // get the state from the db
    let flight_state: i64 =
        match sqlx::query("SELECT f_state FROM flight_state ORDER BY timestamp_ms DESC LIMIT 1")
            .fetch_one(&state.telemetry_db_pool())
            .await
        {
            Ok(data) => data.get::<i64, _>("f_state"),
            Err(_) => FlightState::Startup as i64,
        };
    let flight_state =
        crate::types::u8_to_flight_state(flight_state as u8).unwrap_or(FlightState::Startup);
    Json(flight_state).into_response()
}

/// Returns the backend's current network-synchronized timestamp.
async fn get_network_time(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(NetworkTimeMsg {
        timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
    })
    .into_response()
}

async fn get_launch_clock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.launch_clock_snapshot()).into_response()
}

async fn get_recording_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.recording_status_snapshot()).into_response()
}

/// Returns the current list of operator notifications.
async fn get_notifications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.notifications_snapshot()).into_response()
}

/// Dismisses a persistent notification by id.
async fn dismiss_notification(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    if state.dismiss_notification(id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Returns the current command gating policy for the actions UI.
async fn get_action_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.action_policy_snapshot()).into_response()
}

/// Returns the persisted flight setup profiles and current selection.
async fn get_flight_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(flight_setup::load_or_default()).into_response()
}

/// Persists the current flight setup profile selection and constants.
async fn set_flight_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cfg): Json<FlightSetupConfig>,
) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    match flight_setup::save(&cfg) {
        Ok(()) => Json(flight_setup::load_or_default()).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

/// Returns the persisted nitrogen/nitrous fill targets used by the local sequence logic.
async fn get_fill_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.fill_targets_snapshot()).into_response()
}

/// Persists the local fill targets used by the Ground Station fill sequence.
async fn set_fill_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cfg): Json<FillTargetsConfig>,
) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    match fill_targets::save(&cfg) {
        Ok(()) => {
            let saved = fill_targets::load_or_default();
            state.set_fill_targets(saved.clone());
            Json(saved).into_response()
        }
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

#[derive(Serialize)]
struct FlightSetupApplyResponse {
    selected_profile_id: String,
    wind_level: u8,
    payload_bytes: usize,
}

/// Encodes the current flight setup selection and queues it for the flight side.
async fn apply_flight_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }

    let cfg = flight_setup::load_or_default();
    let now_ms = crate::telemetry_task::get_current_timestamp_ms();
    let profile = match flight_setup::selected_profile(&cfg) {
        Some(profile) => profile,
        None => {
            return (StatusCode::BAD_REQUEST, "selected flight profile missing").into_response();
        }
    };
    let payload = match flight_setup::build_apply_payload(&cfg, now_ms) {
        Ok(payload) => payload,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let Some(router) = state.topology_router.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "router unavailable").into_response();
    };

    match router.log_queue(
        sedsprintf_rs_2026::config::DataType::FlightCommand,
        &payload,
    ) {
        Ok(()) => Json(FlightSetupApplyResponse {
            selected_profile_id: profile.id.clone(),
            wind_level: profile.wind_level,
            payload_bytes: payload.len(),
        })
        .into_response(),
        Err(err) => {
            emit_warning(
                &state,
                format!("Failed to queue flight setup for transmission: {err}"),
            );
            (
                StatusCode::BAD_GATEWAY,
                "failed to queue flight setup for transmission",
            )
                .into_response()
        }
    }
}

/// Returns the synthesized network topology graph shown in the dashboard.
async fn get_network_topology(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.network_topology_snapshot(crate::telemetry_task::get_current_timestamp_ms()))
        .into_response()
}

/// Validates and forwards a frontend command into the backend command channel.
async fn send_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cmd): Json<TelemetryCommand>,
) -> (StatusCode, &'static str) {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => {
            let status = response.status();
            return if status == StatusCode::FORBIDDEN {
                (StatusCode::FORBIDDEN, "permission denied")
            } else {
                (StatusCode::UNAUTHORIZED, "authentication required")
            };
        }
    };
    let cmd_name = command_name(&cmd);
    if !principal.allows_command_name(cmd_name) {
        emit_warning(
            &state,
            format!("Rejected software command {cmd:?}: session is not allowed to send {cmd_name}"),
        );
        return (StatusCode::FORBIDDEN, "command not allowed");
    }
    if !state.is_command_allowed(&cmd) {
        emit_warning(
            &state,
            format!("Ignored software command {cmd:?}: command is currently disabled"),
        );
        return (StatusCode::FORBIDDEN, "command disabled");
    }
    let now_ms = crate::telemetry_task::get_current_timestamp_ms();
    if !state.record_software_command_if_fresh(&cmd, now_ms, software_command_dedup_ms()) {
        println!("Ignored duplicate software command {cmd:?}");
        return (StatusCode::OK, "duplicate ignored");
    }
    let _ = state.cmd_tx.send(cmd).await;
    (StatusCode::OK, "ok")
}

/// Query parameters accepted by the authenticated WebSocket endpoint.
#[derive(Deserialize, Default)]
struct WsAuthQuery {
    token: Option<String>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsAuthQuery>,
) -> impl IntoResponse {
    let principal = match state
        .auth
        .authorize_token(&state.auth_db, query.token.as_deref(), Permission::ViewData)
        .await
    {
        Ok(principal) => principal,
        Err(err) => return auth_failure_response(err),
    };
    ws.on_upgrade(move |socket| handle_ws(socket, state, principal))
}

/// Shape of commands sent from the frontend over WebSocket:
/// { "cmd": "Arm" } or { "cmd": "Disarm" }
#[derive(Deserialize)]
struct WsCommand {
    cmd: TelemetryCommand,
}

async fn handle_ws(socket: WebSocket, state: Arc<AppState>, principal: crate::auth::AuthPrincipal) {
    // Subscribe to all three broadcast channels
    let mut telemetry_rx = state.ws_tx.subscribe();
    let mut warnings_rx = state.warnings_tx.subscribe();
    let mut errors_rx = state.errors_tx.subscribe();
    let mut state_rx = state.state_tx.subscribe();
    let mut launch_clock_rx = state.launch_clock_tx.subscribe();
    let mut board_status_rx = state.board_status_tx.subscribe();
    let mut notifications_rx = state.notifications_tx.subscribe();
    let mut action_policy_rx = state.action_policy_tx.subscribe();
    let mut fill_targets_rx = state.fill_targets_tx.subscribe();
    let mut recording_status_rx = state.recording_status_tx.subscribe();

    let cmd_tx = state.cmd_tx.clone();
    let state_for_send = state.clone();
    let state_for_recv = state.clone();
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
        let initial_notifications = serde_json::to_string(&WsOutMsg::Notifications(
            state_for_send.notifications_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_notifications).await.is_err() {
            return;
        }
        let initial_action_policy = serde_json::to_string(&WsOutMsg::ActionPolicy(
            state_for_send.action_policy_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_action_policy).await.is_err() {
            return;
        }
        let initial_fill_targets = serde_json::to_string(&WsOutMsg::FillTargets(
            state_for_send.fill_targets_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_fill_targets).await.is_err() {
            return;
        }
        let initial_launch_clock = serde_json::to_string(&WsOutMsg::LaunchClock(
            state_for_send.launch_clock_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_launch_clock).await.is_err() {
            return;
        }
        let initial_recording_status = serde_json::to_string(&WsOutMsg::RecordingStatus(
            state_for_send.recording_status_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_recording_status).await.is_err() {
            return;
        }
        let initial_network_topology = serde_json::to_string(&WsOutMsg::NetworkTopology(
            state_for_send
                .network_topology_snapshot(crate::telemetry_task::get_current_timestamp_ms()),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_network_topology).await.is_err() {
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

                recv = launch_clock_rx.recv() => {
                    match recv {
                        Ok(clock) => {
                            let msg = WsOutMsg::LaunchClock(clock);
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
                            let topology = WsOutMsg::NetworkTopology(
                                state_for_send.network_topology_snapshot(
                                    crate::telemetry_task::get_current_timestamp_ms(),
                                ),
                            );
                            let text = serde_json::to_string(&topology).unwrap_or_default();
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

                recv = fill_targets_rx.recv() => {
                    match recv {
                        Ok(targets) => {
                            let msg = WsOutMsg::FillTargets(targets);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = recording_status_rx.recv() => {
                    match recv {
                        Ok(status) => {
                            let msg = WsOutMsg::RecordingStatus(status);
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
                        if !principal.permissions.send_commands {
                            emit_warning(
                                &state_for_recv,
                                format!(
                                    "Rejected websocket command {:?}: authenticated session lacks send-command permission",
                                    cmd.cmd
                                ),
                            );
                            continue;
                        }
                        let cmd_name = command_name(&cmd.cmd);
                        if !principal.allows_command_name(cmd_name) {
                            emit_warning(
                                &state_for_recv,
                                format!(
                                    "Rejected websocket command {:?}: session is not allowed to send {}",
                                    cmd.cmd, cmd_name
                                ),
                            );
                            continue;
                        }
                        if !state_for_recv.is_command_allowed(&cmd.cmd) {
                            emit_warning(
                                &state_for_recv,
                                format!(
                                    "Ignored software command {:?}: command is currently disabled",
                                    cmd.cmd
                                ),
                            );
                            continue;
                        }
                        let now_ms = crate::telemetry_task::get_current_timestamp_ms();
                        if !state_for_recv.record_software_command_if_fresh(
                            &cmd.cmd,
                            now_ms,
                            software_command_dedup_ms(),
                        ) {
                            println!("Ignored duplicate websocket command {:?}", cmd.cmd);
                            continue;
                        }
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
    headers: HeaderMap,
    Query(params): Query<HistoryParams>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let minutes = params.minutes.unwrap_or(20);
    let db = state.telemetry_db_pool();
    let latest_db_ts: Option<i64> = sqlx::query_scalar("SELECT MAX(timestamp_ms) FROM alerts")
        .fetch_one(&db)
        .await
        .ok()
        .flatten();
    let cache_snapshot = state.recent_alerts_snapshot();
    let latest_cache_ts = cache_snapshot.iter().map(|a| a.timestamp_ms).max();

    let now_ms = match (latest_db_ts, latest_cache_ts) {
        (Some(a), Some(b)) => a.max(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return Json(Vec::<AlertDto>::new()).into_response(),
    };
    let cutoff = now_ms - (minutes as i64) * 60_000;

    let alerts_db = sqlx::query(
        r#"
        SELECT timestamp_ms, severity, message
        FROM alerts
        WHERE timestamp_ms BETWEEN ? AND ?
        ORDER BY timestamp_ms DESC
        "#,
    )
    .bind(cutoff)
    .bind(now_ms)
    .fetch_all(&db)
    .await
    .unwrap_or_default();

    let mut alerts: Vec<AlertDto> = alerts_db
        .into_iter()
        .map(|row| AlertDto {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            severity: row.get::<String, _>("severity"),
            message: row.get::<String, _>("message"),
        })
        .collect();

    for alert in cache_snapshot {
        if alert.timestamp_ms < cutoff || alert.timestamp_ms > now_ms {
            continue;
        }
        alerts.push(alert);
    }

    alerts.sort_by(|a, b| b.timestamp_ms.cmp(&a.timestamp_ms));
    alerts.dedup_by(|a, b| {
        a.timestamp_ms == b.timestamp_ms && a.severity == b.severity && a.message == b.message
    });

    Json(alerts).into_response()
}

async fn get_boards(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let now_ms = now_ms_i64().max(0) as u64;
    let msg = state.board_status_snapshot(now_ms);
    Json(msg).into_response()
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
    state.cache_recent_alert(AlertDto {
        timestamp_ms,
        severity: severity.to_string(),
        message: message.clone(),
    });
    let tx = state.db_queue_tx.clone();
    tokio::spawn(async move {
        let _ = tx
            .send(DbQueueItem::Write(DbWrite::Alert {
                timestamp_ms,
                severity: severity.to_string(),
                message,
            }))
            .await;
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
