use crate::auth::{AuthFailure, LoginChallengeRequest, LoginRequest, Permission};
use crate::fill_targets::{self, FillTargetsConfig};
use crate::firmware_update::{self, FirmwareUpdateStatus};
use crate::flight_setup::{self, FlightSetupConfig};
use crate::i18n::{self, TranslateRequest, TranslateResponse, TranslationCatalogResponse};
use crate::layout;
use crate::loadcell;
use crate::map::{detect_max_native_zoom, selected_map_region, tile_bundle_path};
use crate::sequences::{ActionPolicyMsg, PersistentNotification, command_name};
use crate::state::AppState;
use crate::telemetry_db::{DbQueueItem, DbWrite, LaunchClockMsg, RecordingStatusMsg};
use crate::types::{
    Board, BoardStatusMsg, FlightState, NetworkTopologyMsg, TelemetryCommand, TelemetryRow,
};
use axum::body::{Body, to_bytes};
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
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::{OnceCell, mpsc};
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

// NEW

static FAVICON_DATA: OnceCell<Option<Vec<u8>>> = OnceCell::const_new();
static TILE_DB_POOL: OnceCell<sqlx::SqlitePool> = OnceCell::const_new();
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

// Debouncing measures elapsed time, never synchronized network/GPS time.
// A backward clock correction must not suppress commands until it catches up.
fn software_command_clock_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
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
        for chunk in bytes.as_chunks::<4>().0.iter() {
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

#[derive(Default, Deserialize)]
struct RecentDisplayParams {
    #[serde(default)]
    display: bool,
    max_rows: Option<usize>,
}

/// Display-only envelope: balanced history keeps every active stream's newest
/// sample and each bucket's extrema. Recording/export rows are never changed.
fn bounded_display_history(rows: Vec<TelemetryRow>, max_rows: usize) -> Vec<TelemetryRow> {
    let mut streams: HashMap<(String, String), Vec<TelemetryRow>> = HashMap::new();
    for row in rows {
        streams
            .entry((row.sender_id.clone(), row.data_type.clone()))
            .or_default()
            .push(row);
    }
    let mut streams: Vec<_> = streams.into_values().collect();
    // If an adversarial runtime schema alone exceeds the entire envelope,
    // retain its newest streams. Normal applications preserve all streams.
    streams.sort_by_key(|rows| std::cmp::Reverse(rows.last().map(|row| row.timestamp_ms)));
    streams.truncate(max_rows);
    let per_stream = (max_rows / streams.len().max(1)).max(1);
    let mut output = Vec::with_capacity(max_rows);
    for mut stream in streams {
        stream.sort_by_key(|row| row.timestamp_ms);
        if stream.len() <= per_stream {
            output.extend(stream);
            continue;
        }
        if per_stream == 1 {
            output.push(stream.pop().unwrap());
            continue;
        }
        let count = stream.len();
        let mut selected = std::collections::BTreeSet::from([0, count - 1]);
        let buckets = (per_stream.saturating_sub(2) / 2).max(1);
        if per_stream > 2 {
            for bucket in 0..buckets {
                let start = 1 + bucket * (count - 2) / buckets;
                let end = 1 + (bucket + 1) * (count - 2) / buckets;
                let value = |row: &TelemetryRow| {
                    row.values
                        .iter()
                        .flatten()
                        .copied()
                        .find(|v| v.is_finite())
                        .unwrap_or(0.0)
                };
                if let Some((index, _)) = stream[start..end]
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| value(a).total_cmp(&value(b)))
                {
                    selected.insert(start + index);
                }
                if selected.len() < per_stream
                    && let Some((index, _)) = stream[start..end]
                        .iter()
                        .enumerate()
                        .max_by(|(_, a), (_, b)| value(a).total_cmp(&value(b)))
                    {
                        selected.insert(start + index);
                    }
            }
        }
        output.extend(
            stream
                .into_iter()
                .enumerate()
                .filter_map(|(index, row)| selected.contains(&index).then_some(row)),
        );
    }
    output.sort_by_key(|row| row.timestamp_ms);
    output
}

fn recent_rows_for_request(
    rows: Vec<TelemetryRow>,
    cutoff: i64,
    params: &RecentDisplayParams,
) -> Vec<TelemetryRow> {
    let rows = compact_recent_rows(rows, cutoff);
    if params.display {
        bounded_display_history(rows, params.max_rows.unwrap_or(12_000).clamp(512, 20_000))
    } else {
        rows
    }
}

#[cfg(test)]
mod display_history_tests {
    use super::*;
    #[test]
    fn large_reseed_preserves_latest_sparse_pressure_and_extrema_with_a_fixed_payload_bound() {
        let mut input = Vec::new();
        let names = [
            "KG1000",
            "KG50",
            "COMBUSTION_CHAMBER_PRESSURE",
            "COMBUSTION_CHAMBER_VOLTAGE",
            "LOADCELL_WEIGHT_KG",
            "LOADCELL_50_WEIGHT_KG",
            "DAQ_ADC_TEMPERATURE",
            "DAQ_SAR_VOLTAGES",
            "DAQ_CURRENT_SENSE",
            "DAQ_POWER_MONITOR",
            "BATTERY_VOLTAGE",
            "BATTERY_CURRENT",
        ];
        for sample in 0..175_000i64 {
            input.push(TelemetryRow {
                timestamp_ms: sample * 5,
                sender_id: "DAQ".into(),
                data_type: names[(sample % 12) as usize].into(),
                values: vec![Some(if sample == 77_772 {
                    9999.0
                } else {
                    (sample % 100) as f32
                })],
            });
        }
        input.push(TelemetryRow {
            timestamp_ms: 1,
            sender_id: "VB".into(),
            data_type: "FUEL_TANK_PRESSURE".into(),
            values: vec![Some(12.0)],
        });
        input.push(TelemetryRow {
            timestamp_ms: 874_998,
            sender_id: "VB".into(),
            data_type: "FUEL_TANK_PRESSURE".into(),
            values: vec![Some(42.0)],
        });
        input.sort_by_key(|row| row.timestamp_ms);
        let mut latest = HashMap::new();
        for row in &input {
            latest.insert(
                (row.sender_id.clone(), row.data_type.clone()),
                row.timestamp_ms,
            );
        }
        let full =
            recent_rows_for_request(input.clone(), i64::MIN, &RecentDisplayParams::default());
        assert_eq!(
            full.len(),
            input.len(),
            "analysis history remains unchanged without display mode"
        );
        let sampled = recent_rows_for_request(
            input,
            i64::MIN,
            &RecentDisplayParams {
                display: true,
                max_rows: Some(12_000),
            },
        );
        assert!(sampled.len() <= 12_000);
        for ((sender, ty), timestamp) in latest {
            assert!(
                sampled.iter().any(|row| row.sender_id == sender
                    && row.data_type == ty
                    && row.timestamp_ms == timestamp),
                "latest missing: {sender}/{ty}"
            );
        }
        assert_eq!(
            sampled
                .iter()
                .filter(|row| row.data_type == "FUEL_TANK_PRESSURE")
                .count(),
            2
        );
        assert!(sampled.iter().any(|row| row.values == vec![Some(9999.0)]));
        if let Ok(path) = std::env::var("GS_DISPLAY_HISTORY_FIXTURE") {
            std::fs::write(path, serde_json::to_vec(&sampled).unwrap()).unwrap();
        }
    }
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
pub fn router(state: Arc<AppState>, video_password: String) -> Router {
    let voice = Arc::new(crate::voice::VoiceHub::default());
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
        .route("/api/auth/challenge", post(login_challenge))
        .route("/api/auth/login", post(login))
        .route("/api/auth/session", get(get_session_status))
        .route(
            "/api/station/discovery",
            get(crate::station_pairing::discover)
                .options(crate::station_pairing::discovery_preflight),
        )
        .route("/api/auth/logout", post(logout))
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command_entry))
        .route(
            "/api/network-health/daq/sd/retry",
            post(crate::daq_sd::retry),
        )
        .route("/api/rrls/control", get(crate::command_proxy::get_status))
        .route("/api/auto_zero", get(get_auto_zero).post(set_auto_zero))
        .route("/api/auto_zero/launch/cancel", post(cancel_launch_zero))
        .route("/api/alerts", get(get_alerts))
        .route("/api/alert_conditions", get(get_alert_conditions))
        .route("/api/boards", get(get_boards))
        .merge(crate::board_recovery::routes())
        .merge(crate::actuator_plumbing::routes())
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
        .route(
            "/api/calibration/kg50_input",
            get(get_kg50_input_status).post(set_kg50_input),
        )
        .route(
            "/api/calibration/long_zero",
            get(long_zero_status).post(long_zero_start),
        )
        .route("/api/calibration/long_zero/stop", post(long_zero_stop))
        .route("/api/calibration/long_zero/apply", post(long_zero_apply))
        .route(
            "/api/calibration/thermal_settling",
            get(thermal_settling_status),
        )
        .route(
            "/api/calibration/capture_thermal_zero",
            post(capture_thermal_zero),
        )
        .route("/api/calibration/capture_zero", post(capture_loadcell_zero))
        .route("/api/calibration/capture_span", post(capture_loadcell_span))
        .route("/api/calibration/refit", post(refit_loadcell_channel))
        .route(
            "/api/network_time",
            get(get_network_time).post(crate::system_clock::sync_network),
        )
        .route("/api/launch_clock", get(get_launch_clock))
        .route("/api/network_topology", get(get_network_topology))
        .route("/api/network_diagnostics", get(get_network_diagnostics))
        .route(
            "/api/rrls/status",
            get(|| async { Json(crate::rrls::status()) }),
        )
        .route("/api/radio/modem", get(get_radio_modem))
        .route("/api/radio/modem/check", post(post_radio_modem_check))
        .route("/api/firmware/targets", get(get_firmware_targets))
        .route("/api/firmware/updates", get(get_firmware_updates))
        .route("/api/firmware/updates/{id}", get(get_firmware_update))
        .route(
            "/api/firmware/updates/{id}/cancel",
            post(cancel_firmware_update),
        )
        .route("/api/firmware/flash/{board}", post(start_firmware_update))
        .route("/api/recording_status", get(get_recording_status))
        .merge(crate::recording_export::routes())
        .merge(crate::sd_download::routes())
        .route("/api/notifications", get(get_notifications))
        .route("/api/messages", get(get_messages))
        .route(
            "/api/notifications/{id}/dismiss",
            post(dismiss_notification),
        )
        .route("/api/alerts/ack", post(post_alert_ack))
        .route(
            "/api/alerts/history/clear",
            post(post_alert_history_clear),
        )
        .route("/api/alerts/history/clears", get(get_alert_history_clears))
        .route("/api/action_policy", get(get_action_policy))
        .route("/api/board_abort", get(crate::board_abort::get_status))
        .route(
            "/api/board_abort/refresh",
            post(crate::board_abort::refresh_status),
        )
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
        .route("/api/mission_phase", get(get_mission_phase))
        .route("/api/flight_maxima", get(get_flight_maxima))
        .route("/api/gps", get(get_gps))
        .route("/api/antenna/status", get(get_antenna_status))
        .route(
            "/api/station_positions",
            get(crate::antenna::positions::get).post(crate::antenna::positions::set),
        )
        .route("/ws", get(ws_handler))
        .route("/tiles/{z}/{x}/{y}", get(get_tile_jpg))
        .route("/favicon.ico", get(get_favicon))
        .route("/valvestate", get(get_valve_state))
        .merge(if crate::flight_sim_runtime::enabled() {
            crate::flight_sim_runtime::unavailable_media_routes()
        } else {
            crate::media::router(state.clone(), video_password, voice.clone())
        })
        .merge(crate::voice::routes(state.clone(), voice))
        .merge(crate::gse::routes())
        .merge(crate::admin::routes())
        .merge(crate::sequence_access::routes())
        .merge(crate::checklists::routes())
        .merge(crate::client_settings::routes())
        .merge(crate::mission_sync::routes())
        .merge(crate::station_mode::routes())
        .merge(crate::station_mode_switch::routes())
        .merge(crate::station_pairing::manage::routes())
        .merge(crate::launch_history::api::routes())
        // anything that doesn’t match the above routes goes to the static files
        .fallback_service(static_dir)
        .with_state(state)
}

/// Pulls a bearer token out of the Authorization header.
pub(crate) fn bearer_token_from_headers(headers: &HeaderMap) -> Option<&str> {
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
#[expect(clippy::result_large_err, reason = "Axum HTTP helpers retain the explicit Response rejection API without boxing or changing endpoint behavior")]
pub(crate) async fn authorize_headers(
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

fn calibration_edit_forbidden_response() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        "Calibration edit permission required",
    )
        .into_response()
}

fn principal_can_edit_calibration(principal: &crate::auth::AuthPrincipal) -> bool {
    principal.calibration_access.edit
}

/// Creates a one-time login challenge bound to a username.
pub(crate) async fn login_challenge(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LoginChallengeRequest>,
) -> impl IntoResponse {
    match state.auth.create_login_challenge(req).await {
        Ok(response) => Json(response).into_response(),
        Err(err) => auth_failure_response(err),
    }
}

/// Creates a new authenticated session from challenge-bound credentials.
pub(crate) async fn login(
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
    FlightState(FlightStateWire),
    MissionPhase(crate::mission_phase::MissionPhaseMsg),
    FlightMaxima(crate::flight_maxima::FlightMaxima),
    LaunchClock(LaunchClockMsg),
    Error(ErrorMsg),
    AlertAckState(AlertAckStateMsg),
    AlertConditions(crate::alert_conditions::Snapshot),
    BoardAbort(crate::board_abort::Snapshot),
    BoardStatus(BoardStatusMsg),
    NetworkTopology(NetworkTopologyMsg),
    Notifications(Vec<PersistentNotification>),
    Notification(PersistentNotification),
    Messages(Vec<PersistentNotification>),
    ActionPolicy(ActionPolicyMsg),
    FillTargets(FillTargetsConfig),
    RecordingStatus(RecordingStatusMsg),
    NetworkTime(NetworkTimeMsg),
    DashboardReset,
    CalibrationChanged,
    /// The backend switched mode in process:
    /// `{"mode","label","policy_epoch","layout_revision"}`. A fresh
    /// `ActionPolicy` follows; the UI refetches the layout.
    StationModeChanged(serde_json::Value),
}

#[derive(Clone, Serialize)]
pub struct FlightStateMsg {
    pub state: FlightState,
}

/// The WebSocket `FlightState` message under the state-split contract:
/// `state` is the network-level value (set (a)), the GS-only fill sub-phase
/// and the FC's detailed phase ride beside it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FlightStateWire {
    pub state: crate::types::NetworkFlightState,
    pub label: &'static str,
    pub fill_phase: Option<FlightState>,
    pub fill_phase_label: Option<&'static str>,
    pub fc_phase: Option<crate::fc_phase::FcPhaseDto>,
}

impl FlightStateWire {
    pub fn new(local: FlightState, state: &AppState) -> Self {
        let network = local.network();
        let fill_phase = crate::mission_phase::fill_phase(local);
        Self {
            state: network,
            label: network.label(),
            fill_phase,
            fill_phase_label: fill_phase.map(crate::mission_phase::detailed_label),
            fc_phase: state.fc_phase.dto(crate::telemetry_task::get_current_timestamp_ms()),
        }
    }
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
    #[serde(flatten)]
    pub condition:Option<crate::alert_conditions::ConditionLink>,
}

/// Error row sent to frontend (and stored in AppState channel)
#[derive(Clone, Serialize)]
pub struct ErrorMsg {
    pub timestamp_ms: i64,
    pub message: String,
    #[serde(flatten)]
    pub condition:Option<crate::alert_conditions::ConditionLink>,
}

#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct AlertAckStateMsg {
    pub warning_ack_timestamp_ms: i64,
    pub error_ack_timestamp_ms: i64,
    /// Operator-cleared history: events at or before this time are not shown.
    #[serde(default)]
    pub history_cleared_through_ms: i64,
}

/// DTO returned by /api/alerts
/// Frontend expects:
///   [{ "timestamp_ms": i64, "severity": "warning"|"error", "message": "..." }, ...]
#[derive(Clone, Serialize)]
pub struct AlertDto {
    pub timestamp_ms: i64,
    pub severity: String,
    pub message: String,
    #[serde(flatten)]
    pub condition:Option<crate::alert_conditions::ConditionLink>,
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

async fn get_antenna_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(crate::antenna::snapshot()).into_response()
}

/// Returns the latest GPS fix stored in telemetry history.
async fn get_gps(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let live_rocket = {
        let fixes = state.latest_gps_fix_by_sender.lock().unwrap();
        ["RF", "GB", "FC"]
            .iter()
            .filter_map(|sender_id| fixes.get(*sender_id))
            .chain(fixes.values())
            .find_map(|values| gps_point_from_values(values))
    };
    if live_rocket.is_some() {
        return Json(GpsResponse {
            rocket: live_rocket,
        })
        .into_response();
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

    let rocket = row.and_then(|r| gps_point_from_values(&values_from_row(&r)));

    Json(GpsResponse { rocket }).into_response()
}

fn gps_point_from_values(values: &[Option<f32>]) -> Option<GpsPoint> {
    match (value_at(values, 0), value_at(values, 1)) {
        (Some(lat), Some(lon)) => Some(GpsPoint {
            lat: lat as f64,
            lon: lon as f64,
        }),
        _ => None,
    }
}

/// Serves the current frontend layout configuration.
async fn get_layout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    match layout::load_layout() {
        Ok(mut layout) => {
            if !principal.permissions.send_commands {
                layout.actions_tab.actions.clear();
            } else if !principal.command_access.allowed_commands.is_empty() {
                layout.actions_tab.actions.retain(|action| {
                    principal
                        .command_access
                        .allowed_commands
                        .iter()
                        .any(|cmd| cmd == &action.cmd)
                });
            }
            Json(layout).into_response()
        }
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
    State(_state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> impl IntoResponse {
    Json(loadcell::calibration_tab_layout()).into_response()
}

#[derive(Serialize)]
struct MapConfigDto {
    max_native_zoom: u32,
    max_display_zoom: u32,
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
    let max_native_zoom = match detect_max_native_zoom(&selected_map_region()).await {
        Ok(Some(z)) => z,
        Ok(None) => 12,
        Err(e) => {
            eprintln!("WARNING: failed to detect max native zoom: {e:#}");
            12
        }
    };

    Json(MapConfigDto {
        max_native_zoom,
        max_display_zoom: max_native_zoom.saturating_add(1),
        default_center_lat: if crate::rrls::is_rrls() { 43.001 } else { 31.0 },
        default_center_lon: if crate::rrls::is_rrls() {
            -78.790
        } else {
            -99.0
        },
        default_zoom: if crate::rrls::is_rrls() { 12.0 } else { 7.0 },
        map_title: if crate::rrls::is_rrls() {
            "RRLS Map"
        } else {
            "Map"
        }
        .to_string(),
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
struct RefitLoadcellReq {}

/// Returns the in-memory loadcell calibration file.
async fn get_loadcell_calibration(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> impl IntoResponse {
    Json(state.loadcell_calibration.lock().unwrap().clone()).into_response()
}

pub(crate) fn publish_daq_calibration(
    state: &AppState,
    calibration: &loadcell::LoadcellCalibrationFile,
) -> Result<(), String> {
    let router = state
        .topology_router
        .get()
        .ok_or_else(|| "SEDSNet router is not ready".to_string())?;
    crate::network_variables::set_daq_calibration(router, calibration)
        .map_err(|error| format!("publish DAQ calibration: {error}"))
}

/// Saves only the explicitly selected channel; unscoped snapshot replacement
/// is rejected by deserialization, including requests from older frontends.
async fn set_loadcell_calibration(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<loadcell::CalibrationUpdate>,
) -> axum::response::Response {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    let cfg = {
        let mut current = state.loadcell_calibration.lock().unwrap();
        let cfg = match loadcell::merge_channel_update(&current, &update) {
            Ok(cfg) => cfg,
            Err(err) => return (StatusCode::CONFLICT, err).into_response(),
        };
        if let Err(err) = loadcell::save(&cfg) {
            return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
        }
        *current = cfg.clone();
        cfg
    };
    if let Err(err) = publish_daq_calibration(&state, &cfg) {
        return (StatusCode::SERVICE_UNAVAILABLE, err).into_response();
    }
    state.broadcast_fill_targets_snapshot();
    Json(cfg).into_response()
}

#[derive(serde::Deserialize)]
struct Kg50InputRequest {
    input: loadcell::Kg50Input,
}

/// The operator's selection against what the DAQ is reporting right now.
async fn get_kg50_input_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let selected = state.loadcell_calibration.lock().unwrap().kg50_input;
    Json(state.kg50_input_sync.status(selected)).into_response()
}

/// The selection is durable once saved. A send the SDK refuses here is retried
/// from the board's next report, so it does not fail the operator's request.
fn publish_kg50_input(state: &AppState, cfg: &loadcell::LoadcellCalibrationFile) {
    state.kg50_input_sync.note_selected();
    if let Err(error) = publish_daq_calibration(state, cfg) {
        log::warn!("sending DAQ 50 kg input: {error}");
    }
}

async fn set_kg50_input(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<Kg50InputRequest>,
) -> axum::response::Response {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if state.loadcell_processing.status().running {
        return (
            StatusCode::CONFLICT,
            "Stop the zero capture before changing input",
        )
            .into_response();
    }
    if state.recording_status_snapshot().mode != crate::telemetry_db::RecordingModeWire::Idle {
        return (
            StatusCode::CONFLICT,
            "Stop recording before changing the physical load-cell input",
        )
            .into_response();
    }
    let mut cfg = state.loadcell_calibration.lock().unwrap().clone();
    if cfg.kg50_input == req.input {
        publish_kg50_input(&state, &cfg);
        return Json(cfg).into_response();
    }
    loadcell::switch_kg50_input(&mut cfg, req.input);
    cfg.kg50_input_stamp_ms = crate::telemetry_task::get_current_timestamp_ms()
        .max(cfg.kg50_input_stamp_ms.saturating_add(1));
    if let Err(err) = loadcell::save(&cfg) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    *state.loadcell_calibration.lock().unwrap() = cfg.clone();
    state.clear_kg50_samples();
    state.gse.lock().unwrap().observe_mass(None);
    publish_kg50_input(&state, &cfg);
    Json(cfg).into_response()
}

/// Captures a zero point for the selected loadcell sensor.
async fn capture_loadcell_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CaptureLoadcellPointReq>,
) -> impl IntoResponse {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        let temperature = loadcell::recent_adc_temperature(
            state.recent_telemetry_cache.lock().unwrap().iter(),
            "DAQ",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000,
        );
        let Some(raw) =
            loadcell::temperature_corrected_raw(&cfg, &req.sensor_id, req.raw, temperature)
        else {
            return (
                StatusCode::BAD_REQUEST,
                "Fresh ADC temperature and finite raw input required",
            )
                .into_response();
        };
        loadcell::record_capture_temperature(&mut cfg, &req.sensor_id, 0.0, req.raw, temperature);
        loadcell::capture_zero(&mut cfg, &req.sensor_id, raw);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    if let Err(err) = publish_daq_calibration(&state, &updated) {
        return (StatusCode::SERVICE_UNAVAILABLE, err).into_response();
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
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    let updated = {
        let mut cfg = state.loadcell_calibration.lock().unwrap();
        let temperature = loadcell::recent_adc_temperature(
            state.recent_telemetry_cache.lock().unwrap().iter(),
            "DAQ",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64 / 1_000_000,
        );
        let Some(raw) =
            loadcell::temperature_corrected_raw(&cfg, &req.sensor_id, req.raw, temperature)
        else {
            return (
                StatusCode::BAD_REQUEST,
                "Fresh ADC temperature and finite raw input required",
            )
                .into_response();
        };
        loadcell::record_capture_temperature(
            &mut cfg,
            &req.sensor_id,
            req.known_kg,
            req.raw,
            temperature,
        );
        loadcell::capture_span(&mut cfg, &req.sensor_id, raw, req.known_kg);
        cfg.clone()
    };
    if let Err(err) = loadcell::save(&updated) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
    }
    if let Err(err) = publish_daq_calibration(&state, &updated) {
        return (StatusCode::SERVICE_UNAVAILABLE, err).into_response();
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
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let _ = req;
    Json(state.loadcell_calibration.lock().unwrap().clone()).into_response()
}

/// Serves a map tile or a synthesized ancestor fallback when the exact tile is missing.
async fn get_tile_jpg(Path((z, x, y_raw)): Path<(u32, u32, String)>) -> impl IntoResponse {
    let y_trimmed = y_raw.trim_end_matches(".jpg");
    let y: u32 = match y_trimmed.parse() {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if let Some(tile) = read_tile_with_fallback(z, x, y).await {
        let source_value = match tile.source {
            TileResponseSource::Exact => "exact".to_string(),
            TileResponseSource::Fallback { ancestor_z } => format!("fallback:{ancestor_z}"),
        };
        return (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (
                    header::HeaderName::from_static("x-gs26-tile-source"),
                    source_value.as_str(),
                ),
            ],
            tile.bytes,
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// Builds the legacy on-disk tile path for a map tile.
fn tile_path(z: u32, x: u32, y: u32) -> PathBuf {
    crate::map::map_region_base_dir(&selected_map_region()).join(format!("tiles/{z}/{x}/{y}.jpg"))
}

/// Cache successful opens only: downloading map data after startup must work
/// without restarting the service or permanently caching an absent database.
async fn tile_db_pool() -> Option<sqlx::SqlitePool> {
    open_tile_bundle(&TILE_DB_POOL, &tile_bundle_path(&selected_map_region())).await
}

async fn open_tile_bundle(
    cell: &OnceCell<sqlx::SqlitePool>,
    path: &std::path::Path,
) -> Option<sqlx::SqlitePool> {
    if let Some(pool) = cell.get() {
        return Some(pool.clone());
    }
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return None;
    }
    cell.get_or_try_init(|| async {
        let result = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(path)
                    .read_only(true),
            )
            .await;
        if let Err(error) = &result {
            log::warn!("failed to open tile bundle {}: {error}", path.display());
        }
        result
    })
    .await
    .ok()
    .cloned()
}

/// An empty or partially initialized download must not freeze the schema mode.
async fn tile_db_mode(pool: &sqlx::SqlitePool, cell: &OnceCell<TileDbMode>) -> Option<TileDbMode> {
    cell.get_or_try_init(|| async {
        let rows = sqlx::query("PRAGMA table_info(tiles)")
            .fetch_all(pool)
            .await
            .map_err(|_| ())?;
        let has = |name: &str| {
            rows.iter().any(|row| {
                row.try_get::<String, _>("name")
                    .is_ok_and(|column| column == name)
            })
        };
        if has("blob_id") {
            Ok(TileDbMode::Deduped)
        } else if has("image") {
            Ok(TileDbMode::LegacyInline)
        } else {
            Err(())
        }
    })
    .await
    .ok()
    .copied()
}

#[cfg(test)]
mod tile_bundle_lifecycle_tests {
    use super::*;

    #[tokio::test]
    async fn missing_bundle_and_unfinished_schema_are_retried_after_download() {
        let dir = std::env::temp_dir().join(format!(
            "gs-map-late-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("tiles.sqlite3");
        let pool_cell = OnceCell::new();
        let mode_cell = OnceCell::new();
        assert!(open_tile_bundle(&pool_cell, &path).await.is_none());
        assert!(
            !path.exists(),
            "a read request must not create an empty database"
        );
        assert!(pool_cell.get().is_none());
        let writer = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        let reader = open_tile_bundle(&pool_cell, &path).await.unwrap();
        assert!(tile_db_mode(&reader, &mode_cell).await.is_none());
        assert!(mode_cell.get().is_none());
        sqlx::query("CREATE TABLE tiles (z INTEGER, x INTEGER, y INTEGER, blob_id INTEGER)")
            .execute(&writer)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE tile_blobs (id INTEGER PRIMARY KEY, image BLOB)")
            .execute(&writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tile_blobs VALUES (1, X'FFD8FFD9')")
            .execute(&writer)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tiles VALUES (5, 8, 11, 1)")
            .execute(&writer)
            .await
            .unwrap();
        assert!(matches!(
            tile_db_mode(&reader, &mode_cell).await,
            Some(TileDbMode::Deduped)
        ));
        let bytes: Vec<u8> = sqlx::query_scalar("SELECT b.image FROM tiles t JOIN tile_blobs b ON b.id=t.blob_id WHERE z=5 AND x=8 AND y=11")
            .fetch_one(&reader).await.unwrap();
        assert_eq!(bytes, [255, 216, 255, 217]);
        writer.close().await;
        reader.close().await;
        tokio::fs::remove_dir_all(dir).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_inline_bundles_remain_supported() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE tiles (z INTEGER, x INTEGER, y INTEGER, image BLOB)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            tile_db_mode(&pool, &OnceCell::new()).await,
            Some(TileDbMode::LegacyInline)
        ));
        pool.close().await;
    }
}

/// Loads the exact tile from the bundle first and falls back to the legacy filesystem layout.
async fn read_exact_tile(z: u32, x: u32, y: u32) -> Option<Vec<u8>> {
    if let Some(pool) = tile_db_pool().await
        && let Some(mode) = tile_db_mode(&pool, &TILE_DB_MODE).await
    {
        match mode {
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

#[derive(Clone, Debug)]
enum TileResponseSource {
    Exact,
    Fallback { ancestor_z: u32 },
}

#[derive(Clone, Debug)]
struct TileResponse {
    bytes: Vec<u8>,
    source: TileResponseSource,
}

/// Walks up the tile pyramid until it can synthesize a usable child tile.
async fn read_tile_with_fallback(z: u32, x: u32, y: u32) -> Option<TileResponse> {
    if let Some(bytes) = read_exact_tile(z, x, y).await {
        return Some(TileResponse {
            bytes,
            source: TileResponseSource::Exact,
        });
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
            Ok(bytes) => {
                return Some(TileResponse {
                    bytes,
                    source: TileResponseSource::Fallback { ancestor_z: az },
                });
            }
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
async fn get_recent(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<RecentDisplayParams>,
) -> Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    if wants_ndjson(&headers) && !params.display {
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
            let rows = recalibrate_recent_rows(&state, cache_snapshot);
            return Json(recent_rows_for_request(rows, cutoff, &params)).into_response();
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
        let rows = recalibrate_recent_rows(&state, merged_rows);
        return Json(recent_rows_for_request(rows, cutoff, &params)).into_response();
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
    let rows = load_recent_rows_from_db(&state, cutoff, now_ms).await;
    Json(recent_rows_for_request(
        recalibrate_recent_rows(&state, rows),
        cutoff,
        &params,
    ))
    .into_response()
}

fn history_calibration(state: &AppState) -> loadcell::HistoryCalibration {
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    let targets = state.fill_targets_snapshot();
    let flight = *state.state.lock().unwrap();
    loadcell::HistoryCalibration::new(cfg, &targets, flight)
}
fn recalibrate_recent_rows(state: &AppState, mut rows: Vec<TelemetryRow>) -> Vec<TelemetryRow> {
    rows.sort_by_key(|row| row.timestamp_ms);
    let mut replay = history_calibration(state);
    rows.into_iter()
        .flat_map(|row| replay.project(row))
        .collect()
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
            .and_then(|ts| ts.checked_sub(1))
            .or(Some(now_ms))
            .filter(|end_ms| *end_ms >= cutoff)
    };

    let state_for_task = Arc::clone(&state);
    let (tx, rx) = mpsc::channel::<Vec<u8>>(64);
    let mut replay = history_calibration(&state);
    tokio::spawn(async move {
        if let Some(end_ms) = db_end_ms {
            let db = state_for_task.telemetry_db_pool();
            let mut rows = sqlx::query(
                r#"
                SELECT
                    timestamp_ms,
                    source_timestamp_ms,
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
                crate::telemetry_provenance::note(
                    &telemetry_row,
                    row.get::<Option<i64>, _>("source_timestamp_ms")
                        .and_then(|source| {
                            crate::telemetry_provenance::canonical_source(
                                source,
                                crate::telemetry_task::procedure_clock::timestamp_ms(),
                            )
                        }),
                );
                for row in replay.project(telemetry_row) {
                    if send_ndjson_row(&tx, &row).await.is_err() {
                        return;
                    }
                }
            }
        }

        for row in cache_snapshot {
            if row.timestamp_ms < cutoff || row.timestamp_ms > now_ms {
                continue;
            }
            for row in replay.project(row) {
                if send_ndjson_row(&tx, &row).await.is_err() {
                    return;
                }
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
            source_timestamp_ms,
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
        let telemetry_row = TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            sender_id: row.get::<String, _>("sender_id"),
            values: values_from_row(&row),
        };
        crate::telemetry_provenance::note(
            &telemetry_row,
            row.get::<Option<i64>, _>("source_timestamp_ms")
                .and_then(|source| {
                    crate::telemetry_provenance::canonical_source(
                        source,
                        crate::telemetry_task::procedure_clock::timestamp_ms(),
                    )
                }),
        );
        out.push(telemetry_row);
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
/// Returns the same live flight state used by command gates and WebSocket snapshots.
async fn get_flight_state(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    // Recording history may be empty or from a previous run. It must never
    // overwrite the authoritative runtime state with an invented Startup.
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(FlightStateWire::new(state.local_flight_state_snapshot(), &state)),
    )
        .into_response()
}

/// Operator phase (Startup/Idle/Fill/Launch/In flight/Landed) with the
/// detailed shared FlightState kept alongside for the details view.
async fn get_mission_phase(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(crate::mission_phase::refresh(&state)),
    )
        .into_response()
}

/// Current-flight maxima (GS-computed per source, plus FC-reported values).
async fn get_flight_maxima(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(crate::flight_maxima::snapshot()),
    )
        .into_response()
}

#[cfg(test)]
mod live_flight_state_tests {
    use super::*;

    #[tokio::test]
    async fn http_flight_state_matches_runtime_with_empty_recording_history() {
        let mut state = crate::state::tests::test_app_state().await;
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        let auth_path = std::env::temp_dir().join(format!(
            "gs-flight-state-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &auth_path,
            r#"{"version":1,"anonymous":{"view_data":true,"send_commands":false},"users":[]}"#,
        )
        .unwrap();
        Arc::get_mut(&mut state).unwrap().auth =
            Arc::new(crate::auth::AuthManager::new(auth_path.clone()));
        for (expected, network, fill) in [
            (FlightState::Descent, "InFlight", None),
            (FlightState::InFlight, "InFlight", None),
            (FlightState::Startup, "Startup", None),
            (FlightState::NitrousFill, "Fill", Some("NitrousFill")),
            (FlightState::Armed, "Armed", None),
        ] {
            *state.state.lock().unwrap() = expected;
            let response = get_flight_state(State(state.clone()), HeaderMap::new())
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[axum::http::header::CACHE_CONTROL],
                "no-store"
            );
            let body = axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            // Network-level state (set (a)); the fill sub-phase rides beside it.
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["state"], network, "{expected:?}");
            assert_eq!(body["fill_phase"].as_str(), fill, "{expected:?}");
        }
        std::fs::remove_file(auth_path).unwrap();
    }
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
        timestamp_ms: crate::telemetry_task::displayed_network_utc_ms(),
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
    Json(state.launch_clock_presentation_snapshot()).into_response()
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

/// Returns the current list of telemetry/backend messages.
async fn get_messages(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.messages_snapshot()).into_response()
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
    let _ = id;
    StatusCode::NO_CONTENT.into_response()
}

/// Returns the current command gating policy for the actions UI.
async fn get_action_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    crate::sequences::refresh_action_policy_now(&state);
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
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    match flight_setup::save(&cfg) {
        Ok(()) => Json(flight_setup::load_or_default()).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

async fn get_auto_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.auto_zero.lock().unwrap().settings()).into_response()
}

async fn set_auto_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<crate::auto_zero::SettingsUpdate>,
) -> impl IntoResponse {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if crate::gse::active(&state)
        || matches!(
            state.launch_clock_snapshot().kind,
            crate::telemetry_db::LaunchClockKind::TMinus
                | crate::telemetry_db::LaunchClockKind::TPlus
        )
    {
        return (
            StatusCode::CONFLICT,
            "Automatic zero settings cannot change during fill or launch",
        )
            .into_response();
    }
    let mut zero = state.auto_zero.lock().unwrap();
    let settings = zero.settings().updated(update);
    match zero.configure(settings) {
        Ok(()) => Json(settings).into_response(),
        Err(e) => (StatusCode::CONFLICT, e).into_response(),
    }
}

/// Cancels a Launch press held behind the loadcell zero. Launch is not sent.
async fn cancel_launch_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::SendCommands).await {
        return response;
    }
    match crate::auto_zero::before_launch::cancel(&state) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => (StatusCode::CONFLICT, message).into_response(),
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
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    let result = (|| -> Result<FillTargetsConfig, (StatusCode, String)> {
        let mut gse = state.gse.lock().unwrap();
        let zero = state.auto_zero.lock().unwrap();
        let mut current = state.fill_targets.lock().unwrap();
        let source_changed = cfg.fill_source != current.fill_source;
        if source_changed && (gse.engine.active() || zero.pending()) {
            return Err((
                StatusCode::CONFLICT,
                "Cancel the fill/zero capture before changing its loadcell source".into(),
            ));
        }
        let next = fill_targets::canonical_settings(cfg);
        fill_targets::save(&next).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        *current = next.clone();
        if source_changed {
            *state.latest_fill_mass_kg.lock().unwrap() = None;
            gse.observe_mass(None);
        }
        Ok(next)
    })();
    match result {
        Ok(saved) => {
            state.broadcast_fill_targets_snapshot();
            Json(saved).into_response()
        }
        Err((status, err)) => (status, err).into_response(),
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
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };

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
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
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

/// Read-only detail beyond the dashboard's summarized topology.
async fn get_radio_modem(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(crate::comms::rfd_modem::status()).into_response()
}

#[derive(Deserialize)]
struct RadioModemCheckRequest {
    mode: String,
}

/// Explicit operator request to verify (read only) or fix the local RFD modem.
/// Refused in a flight state, during a launch or sequence, or while a check runs.
async fn post_radio_modem_check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<RadioModemCheckRequest>,
) -> Response {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if principal.anonymous {
        return (StatusCode::UNAUTHORIZED, "Sign in to check the radio modem.").into_response();
    }
    if !principal.allows_command_name("RadioModemCheck") {
        return (
            StatusCode::FORBIDDEN,
            "RadioModemCheck command permission is required.",
        )
            .into_response();
    }
    let mode = if request.mode.trim().eq_ignore_ascii_case("reset") {
        crate::comms::rfd_modem::Mode::Reset
    } else {
        match crate::comms::rfd_modem::Mode::parse(&request.mode) {
            Some(mode) => mode,
            None => {
                return (StatusCode::BAD_REQUEST, "mode must be verify, fix or reset")
                    .into_response();
            }
        }
    };
    // Re-evaluate now rather than trusting the 250 ms refresh.
    crate::comms::rfd_modem::set_gate(crate::comms::rfd_modem::station_block_reason(&state));
    match crate::comms::rfd_modem::request_operator_check(mode) {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"accepted": true})),
        )
            .into_response(),
        Err(reason) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"accepted": false, "reason": reason})),
        )
            .into_response(),
    }
}

async fn get_network_diagnostics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let Some(router) = state.topology_router.get() else {
        return Json(serde_json::json!({"ready": false})).into_response();
    };
    let stats = router.export_runtime_stats();
    let topology = router.export_topology();
    let sides: Vec<_> = stats
        .sides
        .iter()
        .map(|side| {
            let types: Vec<_> = side
                .data_types
                .iter()
                .map(|ty| {
                    serde_json::json!({
                        "data_type": ty.data_type.as_u32(), "rx_packets": ty.rx_packets,
                        "tx_packets": ty.tx_packets, "tx_retries": ty.tx_retries,
                        "handler_failures": ty.handler_failures,
                    })
                })
                .collect();
            serde_json::json!({"name": side.side_name, "rx_packets": side.rx_packets,
            "tx_packets": side.tx_packets, "tx_retries": side.tx_retries,
            "handler_failures": side.tx_handler_failures,
            "template_evictions": side.side_transport_template_evictions,
            "rx_templates": side.side_transport_rx_template_count, "data_types": types})
        })
        .collect();
    let routes: Vec<_> = topology.routes.iter().map(|route| {
        let announcers: Vec<_> = route.announcers.iter().map(|peer| serde_json::json!({
            "sender": peer.sender_id, "age_ms": peer.age_ms,
            "endpoints": peer.reachable_endpoints.iter().map(|ep| format!("{ep:?}")).collect::<Vec<_>>(),
            "variables": peer.reachable_network_variables.iter().map(|ty| ty.as_u32()).collect::<Vec<_>>(),
        })).collect();
        serde_json::json!({"side": route.side_name, "age_ms": route.age_ms, "announcers": announcers})
    }).collect();
    Json(serde_json::json!({"ready": true, "sides": sides, "routes": routes,
        "queues": {"rx_len": stats.queues.rx_len, "tx_len": stats.queues.tx_len,
            "shared_bytes": stats.queues.shared_queue_bytes_used, "replay_len": stats.queues.replay_len},
        "reliable": {"pending": stats.reliable.end_to_end_pending_count,
            "pending_destinations": stats.reliable.end_to_end_pending_destination_count},
        "handler_failures": stats.total_handler_failures,
        "handler_retries": stats.total_handler_retries,
        "discovery": {"interval_ms": stats.discovery.current_announce_interval_ms,
            "next_announce_ms": stats.discovery.next_announce_ms},
        "radio_tx_budget": crate::telemetry_task::radio_tx_budget_diagnostics(),
    })).into_response()
}

#[derive(Serialize)]
struct FirmwareTarget {
    board: String,
    board_label: String,
    hostname: String,
    discovered: bool,
    supported: bool,
    ota_stream_port: u16,
    artifact_kind: &'static str,
    workflow: &'static str,
    unavailable_reason: Option<&'static str>,
}

/// Lists every board that can be addressed by the common LaunchCore OTA stream protocol.
async fn get_firmware_targets(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let router = state.topology_router.get();
    let targets = Board::ALL
        .iter()
        .copied()
        .filter(|board| *board != Board::GroundStation)
        .map(|board| FirmwareTarget {
            board: board.sender_id().to_string(),
            board_label: board.as_str().to_string(),
            hostname: board.sender_id().to_string(),
            discovered: router
                .is_some_and(|router| router.resolve_hostname(board.sender_id()).is_some()),
            supported: firmware_update::supports_live_ota(board),
            ota_stream_port: firmware_update::OTA_STREAM_PORT,
            artifact_kind: "launchcore_delta",
            workflow: if firmware_update::supports_live_ota(board) {
                "sedsnet_delta"
            } else {
                "unavailable"
            },
            unavailable_reason: (!firmware_update::supports_live_ota(board)).then_some(
                "current board firmware does not implement the live SEDSnet OTA receiver",
            ),
        })
        .collect::<Vec<_>>();
    Json(targets).into_response()
}

/// Returns recent firmware jobs so clients can restore progress after reconnecting.
async fn get_firmware_updates(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(firmware_update::update_history()).into_response()
}

async fn get_firmware_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    match firmware_update::update_status(id) {
        Some(status) => Json(status).into_response(),
        None => (StatusCode::NOT_FOUND, "firmware update not found").into_response(),
    }
}

/// Accepts a `.seds` firmware artifact and starts an asynchronous SEDSnet stream transfer.
async fn start_firmware_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(board): Path<String>,
    body: Body,
) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ground station is shutting down",
        )
            .into_response();
    }
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    if !principal.allows_command_name("FirmwareUpdate") {
        return (StatusCode::FORBIDDEN, "firmware update not allowed").into_response();
    }
    let Some(board) = firmware_update::parse_board_target(&board) else {
        return (StatusCode::BAD_REQUEST, "unknown firmware target board").into_response();
    };
    let Some(filename) = headers
        .get("x-firmware-filename")
        .and_then(|value| value.to_str().ok())
    else {
        return (
            StatusCode::BAD_REQUEST,
            "X-Firmware-Filename is required and must end in .seds",
        )
            .into_response();
    };
    let filename = match firmware_update::validate_firmware_filename(filename) {
        Ok(filename) => filename,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let firmware = match to_bytes(body, firmware_update::MAX_UPLOAD_BYTES).await {
        Ok(bytes) => bytes.to_vec(),
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                "firmware upload exceeds the server limit",
            )
                .into_response();
        }
    };
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ground station is shutting down",
        )
            .into_response();
    }
    let Some(router) = state.topology_router.get() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "router unavailable").into_response();
    };
    match firmware_update::start_update(router.clone(), board, filename, firmware) {
        Ok(status) => (StatusCode::ACCEPTED, Json(status)).into_response(),
        Err(err) if err.contains("already active") => (StatusCode::CONFLICT, err).into_response(),
        Err(err) if err.contains("not present") => {
            (StatusCode::SERVICE_UNAVAILABLE, err).into_response()
        }
        Err(err) => (StatusCode::BAD_REQUEST, err).into_response(),
    }
}

async fn cancel_firmware_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !principal.permissions.send_commands {
        return (StatusCode::FORBIDDEN, "permission denied").into_response();
    }
    if !principal.allows_command_name("FirmwareUpdate") {
        return (StatusCode::FORBIDDEN, "firmware update not allowed").into_response();
    }
    match firmware_update::cancel_update(id) {
        Ok(status) => Json::<FirmwareUpdateStatus>(status).into_response(),
        Err(err) if err.contains("not found") => (StatusCode::NOT_FOUND, err).into_response(),
        Err(err) => (StatusCode::CONFLICT, err).into_response(),
    }
}

/// RRLS forwards opaque operation names, so a normal RRLS build does not need
/// to compile primary HITL variants or execute them locally.
async fn send_command_entry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(value): Json<serde_json::Value>,
) -> Response {
    let Some(name) = value.as_str() else {
        return (StatusCode::BAD_REQUEST, "Expected a command name.").into_response();
    };
    let plumbing_source_ms = if name == "RetractPlumbing" {
        state
            .topology_router
            .get()
            .and_then(|r| crate::system_clock::required_control_network_utc_ms(r).ok())
    } else {
        None
    };
    let remote_launch_origin = crate::command_proxy::capture_rrls_launch_origin(&state, name);
    if crate::rrls::is_rrls() && !crate::command_proxy::rrls_runs_locally(name) {
        let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(principal) => principal,
            Err(error) => return error,
        };
        return crate::command_proxy::submit_with_origins(
            &state,
            &principal,
            bearer_token_from_headers(&headers),
            crate::command_proxy::Operation::Command(name.into()),
            plumbing_source_ms,
            remote_launch_origin,
        )
        .await;
    }
    let command = match serde_json::from_value(value) {
        Ok(command) => command,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "Unknown command for this station build.",
            )
                .into_response();
        }
    };
    send_command(State(state), headers, Json(command))
        .await
        .into_response()
}

fn command_disabled_message(state:&AppState,command:&TelemetryCommand)->String {
    let name=command_name(command);
    let phase_step=match name {
        "AdvanceFlightState"=>Some(true),
        "RewindFlightState"=>Some(false),
        _=>match command {
            TelemetryCommand::OperatorPhase{intent}=>Some(intent.name()=="AdvanceFlightState"),
            _=>None,
        },
    };
    if let Some(forward)=phase_step && crate::operator_phase::local_mode(state) {
        return format!("{name}: {}",crate::operator_phase::refusal_reason(state,forward));
    }
    if let Some(reason)=crate::sequence_access::refusal(state,command) {
        return format!("{name}: {reason}");
    }
    let policy=state.action_policy_snapshot();
    let reason=policy.controls.iter().find(|control|control.cmd==name)
        .and_then(|control|control.disabled_reason.clone())
        .or_else(||crate::gse::command_refusal_reason(state,command))
        .or_else(||(crate::runtime_mode::is_hitl()&&!state.hitl_button_interlock_satisfied())
            .then(||"Enable the HITL button interlock.".to_string()));
    reason.map_or_else(||format!("{name} is unavailable under the current GroundStation policy. Check its control details."),|reason|format!("{name}: {reason}"))
}

/// Admits a Launch press against station mode changes: the returned hold
/// travels with the queued command, and a committed mode change refuses it.
fn admit_launch_press(
    state: &AppState,
    cmd: &TelemetryCommand,
) -> Result<Option<crate::launch_mode_gate::LaunchHold>, String> {
    if !matches!(cmd, TelemetryCommand::Launch | TelemetryCommand::GroundStationLaunch) {
        return Ok(None);
    }
    state
        .launch_mode_gate
        .admit()
        .map(Some)
        .map_err(|reason| format!("{}: {reason}", command_name(cmd)))
}

/// Why a Launch press must be refused at once, matching the action policy's
/// launch-journal and turbocharge reasons. The send path checks these again.
fn launch_press_block(state: &AppState, cmd: &TelemetryCommand) -> Option<&'static str> {
    if crate::rrls::is_rrls() || !matches!(command_name(cmd), "Launch" | "GroundStationLaunch") {
        return None;
    }
    if crate::gse::turbocharge_active_now(state) {
        return Some(crate::gse::TURBOCHARGE_UNFINISHED_MESSAGE);
    }
    crate::gse::launch_journal_block(&state.launch_history.lock().unwrap())
}

/// While the operator Abort indicator is latched, gse::queue_controller drops
/// every controller packet except the abort-safe directions. Refuse such a HITL
/// flight command here with that reason instead of answering "ok" and
/// dropping it at send time. This reports the existing rule; it does not
/// decide whether the command should be allowed after an Abort.
fn abort_fence_block(state: &AppState, cmd: &TelemetryCommand) -> Option<String> {
    if !state.abort_indicator_latched() || crate::rrls::is_rrls() {
        return None;
    }
    let byte = crate::telemetry_task::hitl_flight_command_id(cmd)?;
    let payload = [byte];
    if crate::gse::abort_safe_control(crate::telemetry_schema::data_type("FLIGHT_COMMAND"), &payload) {
        return None;
    }
    Some(format!(
        "{} refused: the operator Abort is latched, and the station sends no flight-computer command except Evaluation Abort until Reset all clears it",
        command_name(cmd)
    ))
}

/// Validates and forwards a frontend command into the backend command channel.
async fn send_command(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cmd): Json<TelemetryCommand>,
) -> (StatusCode, String) {
    let Some(lease) = state.control_ingress.handler() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Control ingress is closed".into(),
        );
    };
    lease
        .run(send_command_inner(State(state), headers, Json(cmd)))
        .await
}

async fn send_command_inner(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(cmd): Json<TelemetryCommand>,
) -> (StatusCode, String) {
    let cmd = crate::operator_phase::seal(&state,cmd);
    let safe_manual = crate::operator_queue::safe_manual::Intent::capture(&state, &cmd);
    let recovery_origin = crate::operator_queue::manual_origin::Origin::capture(&state,&cmd);
    let fill_origin = matches!(cmd, TelemetryCommand::StartFill)
        .then(|| crate::fill_recording::Captured::capture(&state));
    // Request origin must precede authentication/database waits across recovery.
    let admission_generation = if matches!(cmd, TelemetryCommand::Abort) {
        0
    } else {
        crate::gse::capture_operator_origin(&state).or_else(||recovery_origin.map(|origin|origin.generation())).unwrap_or(0)
    };
    let plumbing_origin = crate::actuator_plumbing::capture_motion_origin(&state, &cmd);
    let launch_revision = crate::board_abort::capture_launch_revision(&state, &cmd);
    #[cfg(test)]
    if !crate::telemetry_task::recording_owner::test_hooks::before_http_auth(&state).await {
        return crate::fill_recording::http_result(Err(crate::fill_recording::Error::Stale));
    }
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => {
            let status = response.status();
            return if status == StatusCode::FORBIDDEN {
                (StatusCode::FORBIDDEN, "permission denied".into())
            } else {
                (StatusCode::UNAUTHORIZED, "authentication required".into())
            };
        }
    };
    if let Some(intent) = safe_manual {
        return crate::operator_queue::safe_manual::dispatch(state, &principal, intent, None).await;
    }
    let cmd = if let Some(captured) = fill_origin {
        let weak = Arc::downgrade(&state);
        let authority = crate::fill_recording::Authority::Session {
            token: bearer_token_from_headers(&headers).map(str::to_owned),
            actor: principal.username.clone(),
            admission_current: Arc::new(move || {
                weak.upgrade().map_or(crate::gse::CurrentState::Invalid, |state| fill_admission_state(&state))
            }),
        };
        TelemetryCommand::FillWithRecording {
            origin: captured.bind(authority, Some(&principal)),
        }
    } else {
        cmd
    };
    dispatch_command_inner(
        state, principal, cmd, false, admission_generation,
        CommandDispatchOrigins { plumbing_origin, launch_revision, recovery_origin },
    )
    .await
}

/// `is_command_allowed`, except that a refusal only because a StartFill
/// resume probe found the SDK core held is checked again, briefly.
pub(crate) async fn command_allowed_waiting_out_core(state: &AppState, cmd: &TelemetryCommand) -> bool {
    crate::core_busy::retry(
        || if state.is_command_allowed(cmd) { Ok(()) } else { Err(()) },
        |_| crate::gse::command_gate_core_busy(state, cmd),
    )
    .await
    .is_ok()
}
/// A recorded Fill's live admission. A held SDK core is busy, never stale, so
/// the Fill owner retries instead of dropping the operator's Fill.
fn fill_admission_state(state: &AppState) -> crate::gse::CurrentState {
    if state.is_command_allowed(&TelemetryCommand::StartFill) {
        crate::gse::CurrentState::Current
    } else if crate::gse::command_gate_core_busy(state, &TelemetryCommand::StartFill) {
        crate::gse::CurrentState::PendingBusy
    } else {
        crate::gse::CurrentState::Invalid
    }
}
/// Shared primary/local authorization and dispatch. A proxy must supply a
/// freshly resolved local principal; this never changes RRLS execution policy.
#[cfg(test)]
pub(crate) async fn dispatch_authorized_command(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    cmd: TelemetryCommand,
    admission_generation: u64,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
) -> (StatusCode, String) {
    dispatch_command(
        state,
        principal,
        cmd,
        false,
        admission_generation,
        plumbing_origin,
        launch_revision,
    )
    .await
}
pub(crate) async fn dispatch_proxied_command(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    cmd: TelemetryCommand,
    launch_revision: Option<u64>,
) -> (StatusCode, String) {
    let admission_generation = if matches!(cmd, TelemetryCommand::Abort) {
        0
    } else {
        crate::gse::capture_operator_origin(&state).unwrap_or(0)
    };
    dispatch_command(
        state,
        principal,
        cmd,
        true,
        admission_generation,
        None,
        launch_revision,
    )
    .await
}
pub(crate) async fn dispatch_proxied_command_with_origin(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    cmd: TelemetryCommand,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
) -> (StatusCode, String) {
    let admission_generation = if matches!(cmd, TelemetryCommand::Abort) {
        0
    } else {
        crate::gse::capture_operator_origin(&state).unwrap_or(0)
    };
    dispatch_command(
        state,
        principal,
        cmd,
        true,
        admission_generation,
        plumbing_origin,
        launch_revision,
    )
    .await
}

async fn dispatch_command(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    cmd: TelemetryCommand,
    immediate_admission: bool,
    admission_generation: u64,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
) -> (StatusCode, String) {
    let Some(lease) = state.control_ingress.handler() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Control ingress is closed".into(),
        );
    };
    lease
        .run(dispatch_command_inner(
            state,
            principal,
            cmd,
            immediate_admission,
            admission_generation,
            CommandDispatchOrigins { plumbing_origin, launch_revision, recovery_origin: None },
        ))
        .await
}

/// The original command's provenance moves as one value across the handler
/// future; this groups existing fields without recapturing any authority.
struct CommandDispatchOrigins {
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
    recovery_origin: Option<crate::operator_queue::manual_origin::Origin>,
}
async fn dispatch_command_inner(
    state: Arc<AppState>,
    principal: crate::auth::AuthPrincipal,
    cmd: TelemetryCommand,
    immediate_admission: bool,
    admission_generation: u64,
    origins: CommandDispatchOrigins,
) -> (StatusCode, String) {
    let CommandDispatchOrigins { plumbing_origin, launch_revision, mut recovery_origin } = origins;
    let cmd_name = command_name(&cmd);
    if !principal.allows_command_name(cmd_name) {
        emit_warning(
            &state,
            format!("Rejected software command {cmd:?}: session is not allowed to send {cmd_name}"),
        );
        return (StatusCode::FORBIDDEN, "command not allowed".into());
    }
    // Permission is resolved, but no ordinary-queue/GSE/disk wait may precede
    // retention of a new manual emergency stop. The primary owns execution.
    if matches!(cmd, TelemetryCommand::Abort) && !crate::rrls::is_rrls() {
        // Only the existing proxy wrappers select immediate_admission. Their
        // secondary Stop has no proven original UTC and must never be restamped.
        if immediate_admission {
            state.manual_abort_intent.request_untimed();
        } else {
            state.manual_abort_intent.request();
        }
        return (
            StatusCode::ACCEPTED,
            "manual Abort intent retained; board readback required".into(),
        );
    }
    if crate::runtime_mode::is_hitl()
        && matches!(cmd,TelemetryCommand::AdvanceFlightState|TelemetryCommand::RewindFlightState) {
        return (StatusCode::CONFLICT,"Phase steps require a fresh request on the primary GroundStation connection.".into());
    }
    if !crate::operator_phase::is_local(&cmd) && crate::board_abort::launch_command(command_name(&cmd)) {
        if let Err(error) = crate::board_abort::validate_launch_revision(&state, launch_revision) {
            return (StatusCode::CONFLICT, error);
        }
        if let Some(message) = crate::board_abort::blocked_message(&state) {
            emit_notification_warning(&state, &message);
            return (StatusCode::CONFLICT, message);
        }
    }
    // Phase-step gates read shared state with try_lock; a lock that is merely
    // busy at this instant must not refuse a valid step. Re-check briefly.
    let mut allowed=command_allowed_waiting_out_core(&state,&cmd).await;
    if !allowed && crate::operator_phase::is_local(&cmd) {
        for _ in 0..5 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if state.is_command_allowed(&cmd) { allowed=true; break; }
        }
    }
    if !allowed {
        let message=command_disabled_message(&state,&cmd);
        emit_notification_warning(&state,&message);
        return (StatusCode::FORBIDDEN, message);
    }
    // The launch journal (a real earlier attempt, or an unfinished board
    // recovery) and an active turbocharge disable Launch in the action
    // policy. Refuse the press here with that reason instead of answering
    // "ok" and dropping it at send time.
    if let Some(reason) = launch_press_block(&state, &cmd) {
        let message = format!("{cmd_name}: {reason}");
        emit_notification_warning(&state, &message);
        return (StatusCode::FORBIDDEN, message);
    }
    if let Some(message) = abort_fence_block(&state, &cmd) {
        log::warn!("{message}");
        emit_notification_warning(&state, &message);
        return (StatusCode::CONFLICT, message);
    }
    if matches!(cmd, TelemetryCommand::RetractPlumbing)
        && let Err(message) =
            crate::actuator_plumbing::validate_motion_origin(&state, plumbing_origin)
        {
            emit_notification_warning(&state, message);
            return (
                StatusCode::CONFLICT,
                "plumbing motion context is stale or unavailable".into(),
            );
        }
    // A bounded proxy must not wait behind a full command channel after its
    // signed contract expires, or record an unadmitted intent as a duplicate.
    let permit = if immediate_admission && !matches!(cmd, TelemetryCommand::ResetTClock) {
        match state.cmd_tx.clone().try_reserve_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "primary command queue is busy".into(),
                );
            }
        }
    } else {
        None
    };
    // From here a Launch holds station mode changes, and a committed mode
    // change refuses it (one lock; see launch_mode_gate).
    let launch_hold = match admit_launch_press(&state, &cmd) {
        Ok(hold) => hold,
        Err(message) => {
            emit_notification_warning(&state, &message);
            return (StatusCode::CONFLICT, message);
        }
    };
    let now_ms = software_command_clock_ms();
    let mut admission_rejection=None;
    let duplicate = match crate::gse::record_operator_command_with_recovery_origin_diagnostic(
        &state,
        admission_generation,
        &cmd,
        now_ms,
        software_command_dedup_ms(),
        launch_revision,
        crate::gse::RecoveryAdmissionDiagnostic {
            origin: &mut recovery_origin,
            rejection: &mut admission_rejection,
        },
    ) {
        Err(message) => return (StatusCode::CONFLICT, message),
        Ok(crate::gse::OperatorCommandAdmission::Stale) => {
            let message=admission_rejection.unwrap_or(crate::operator_queue::manual_origin::Rejection::MissingOrigin).message();
            log::warn!("Command {} was not admitted: {}",cmd_name,message);
            return (StatusCode::CONFLICT, message.into());
        }
        Ok(crate::gse::OperatorCommandAdmission::Duplicate) => true,
        Ok(crate::gse::OperatorCommandAdmission::Fresh) => false,
    };
    // A pending/failed recording preparation is not a completed Fill. Never
    // acknowledge that duplicate as recording-ready; a fresh explicit retry
    // may retry storage once the bounded preparation owner has finished.
    let fill_retry = matches!(cmd, TelemetryCommand::FillWithRecording { .. })
        && state.fill_recording.ready_instance() == 0;
    if duplicate && fill_retry && state.fill_recording.preparation.available_permits() == 0 {
        return crate::fill_recording::http_result(Err(crate::fill_recording::Error::Busy));
    }
    if duplicate && !fill_retry {
        gs_debug_println!("Ignored duplicate software command {cmd:?}");
        if matches!(cmd, TelemetryCommand::ResetTClock) {
            if crate::rrls::is_rrls() {
                return match crate::launch_clock_sync::await_existing_reset().await {
                    Ok(true) => (StatusCode::OK, "primary T clock reset saved".into()),
                    Ok(false) => (
                        StatusCode::ACCEPTED,
                        "primary reset pending; retained clock unchanged".into(),
                    ),
                    Err(_) => (
                        StatusCode::CONFLICT,
                        "primary reset was not confirmed".into(),
                    ),
                };
            }
            let waiter = state.launch_history.lock().unwrap().waiter();
            if crate::launch_history::flush(waiter).await.is_err() {
                return (
                    StatusCode::INSUFFICIENT_STORAGE,
                    "T clock reset was not saved".into(),
                );
            }
        }
        return (StatusCode::OK, "duplicate ignored".into());
    }
    if matches!(cmd, TelemetryCommand::ResetTClock) {
        if crate::rrls::is_rrls() {
            return match crate::launch_clock_sync::request_reset(&state).await {
                Ok(true) => (StatusCode::OK, "primary T clock reset saved".into()),
                Ok(false) => (
                    StatusCode::ACCEPTED,
                    "primary reset pending; retained clock unchanged".into(),
                ),
                Err(error) => {
                    emit_notification_warning(
                        &state,
                        format!("Primary T clock reset not confirmed: {error}"),
                    );
                    (
                        StatusCode::CONFLICT,
                        "primary reset was not confirmed".into(),
                    )
                }
            };
        }
        let work = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            reset_primary_clock_for_origin(&work, admission_generation)
        })
        .await
        .unwrap_or_else(|error| Err((StatusCode::CONFLICT, error.into())));
        if let Err((status, error)) = result {
            if status == StatusCode::INSUFFICIENT_STORAGE {
                emit_notification_warning(&state, format!("T clock reset is not durable: {error}"));
                return (
                    StatusCode::INSUFFICIENT_STORAGE,
                    "T clock reset was not saved".into(),
                );
            }
            emit_notification_warning(&state, format!("T clock reset failed: {error}"));
            return (StatusCode::CONFLICT, "T clock reset failed".into());
        }
        return (StatusCode::OK, "ok".into());
    }
    if let TelemetryCommand::FillWithRecording { origin } = cmd {
        drop(permit);
        let result = crate::fill_recording::start(state.clone(), origin).await;
        if let Err(error) = &result {
            crate::fill_recording::notify_failure(&state, error);
        }
        return crate::fill_recording::http_result(result);
    }
    if matches!(cmd, TelemetryCommand::StartFill) {
        return crate::fill_recording::http_result(Err(crate::fill_recording::Error::Stale));
    }
    if let Some(permit) = permit {
        permit.send(TelemetryCommand::QueuedOperator {
            launch_hold,
            recovery_origin,
            launch_revision,
            plumbing_origin,
            generation: admission_generation,
            command: Box::new(cmd),
        });
        return (StatusCode::OK, "ok".into());
    }
    if state
        .cmd_tx
        .send(TelemetryCommand::QueuedOperator {
            launch_hold,
            recovery_origin,
            launch_revision,
            plumbing_origin,
            generation: admission_generation,
            command: Box::new(cmd),
        })
        .await
        .is_err()
    {
        emit_notification_warning(&state, "Command worker unavailable; command was not queued");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "command worker unavailable".into(),
        );
    }
    (StatusCode::OK, "ok".into())
}

/// Called inside the actual blocking worker, never around its async caller.
/// HTTP/WS cancellation must not release the worker's mutation or durability
/// wait. The claim is a lease; no dispatch/history mutex crosses writer.wait.
fn reset_primary_clock_for_origin(
    state: &AppState,
    generation: u64,
) -> Result<(), (StatusCode, anyhow::Error)> {
    #[cfg(test)]
    let _completion = clock_worker_tests::completion(state);
    #[cfg(test)]
    clock_worker_tests::pause_worker(state, clock_worker_tests::Phase::BeforeClaim);
    let _claim = crate::gse::claim_operator_dispatch(state, generation).ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            anyhow::anyhow!("T clock reset cancelled by recovery"),
        )
    })?;
    #[cfg(test)]
    clock_worker_tests::pause_worker(state, clock_worker_tests::Phase::BeforeMutation);
    state
        .try_reset_launch_clock()
        .map_err(|error| (StatusCode::CONFLICT, error))?;
    #[cfg(test)]
    clock_worker_tests::pause_worker(state, clock_worker_tests::Phase::AfterMutation);
    let waiter = state.launch_history.lock().unwrap().waiter();
    if let Some((writer, ticket)) = waiter {
        // A timeout reports failure, not writer quiescence. Recovery must still
        // verify the actual journal prefix before publishing its completion.
        writer
            .wait(ticket)
            .map_err(|error| (StatusCode::INSUFFICIENT_STORAGE, error))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod command_dispatch_tests {
    use super::*;
    use crate::recording_export::tests::{TestDirectory, export_state};

    #[test]
    fn every_advertised_button_command_has_a_matching_request_variant() {
        for name in crate::sequences::all_command_names() {
            let cmd: TelemetryCommand = serde_json::from_value(serde_json::json!(name))
                .unwrap_or_else(|error| {
                    panic!("advertised command {name} has no request variant: {error}")
                });
            assert_eq!(command_name(&cmd), name);
        }
        let layout = if crate::runtime_mode::is_hitl() {
            include_str!("../layout/layout_hitl.json")
        } else if crate::runtime_mode::is_test_fire() {
            include_str!("../layout/layout_test_fire.json")
        } else {
            include_str!("../layout/layout.json")
        };
        fn check(value: &serde_json::Value) -> usize {
            match value {
                serde_json::Value::Object(fields) => {
                    let mut count = 0;
                    if let Some(serde_json::Value::String(name)) = fields.get("cmd") {
                        let cmd: TelemetryCommand = serde_json::from_value(serde_json::json!(name))
                            .unwrap_or_else(|error| {
                                panic!("configured UI command {name} is invalid: {error}")
                            });
                        assert_eq!(command_name(&cmd), name);
                        count += 1;
                    }
                    count + fields.values().map(check).sum::<usize>()
                }
                serde_json::Value::Array(values) => values.iter().map(check).sum(),
                _ => 0,
            }
        }
        assert!(
            check(&serde_json::from_str(layout).unwrap()) > 0,
            "the shipped layout must advertise controls"
        );
    }

    /// A Launch press that the launch journal blocks (a real earlier attempt
    /// or an unfinished board recovery) is refused with the action policy's
    /// reason, never answered "ok" and dropped at send time.
    #[tokio::test]
    async fn launch_press_is_refused_with_the_launch_journal_reason() {
        let state = crate::state::tests::test_app_state().await;
        assert_eq!(launch_press_block(&state, &TelemetryCommand::Launch), None);
        state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        assert_eq!(launch_press_block(&state, &TelemetryCommand::Launch), Some(crate::gse::LAUNCH_LOCKED_MESSAGE));
        assert_eq!(launch_press_block(&state, &TelemetryCommand::Abort), None, "Abort is never refused by it");
    }

    #[tokio::test]
    async fn http_command_reports_closed_worker_instead_of_acknowledging_a_lost_request() {
        let root = TestDirectory::new();
        let mut state = export_state(&root.0, false).await;
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::json!({"users":[{
                "username":"operator", "roles":[], "password":{"salt_b64":"","hash_b64":""},
                "permissions":{"view_data":true,"send_commands":true}
            }]})
            .to_string(),
        )
        .unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('command-test','operator','session',1,1,0,9999999999999)")
            .execute(&state.auth_db).await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer command-test".parse().unwrap(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        Arc::get_mut(&mut state).unwrap().cmd_tx = tx;
        let result = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Postinit),
        )
        .await;
        assert_eq!(result.0, StatusCode::OK);
        let queued = rx.try_recv().unwrap();
        assert!(
            matches!(queued, TelemetryCommand::QueuedOperator { generation, command , .. }
            if generation == crate::gse::automatic_generation(&state)
                && matches!(*command, TelemetryCommand::Postinit))
        );
        // Full ordinary capacity cannot delay authenticated emergency retention.
        for _ in 0..4 {
            state.cmd_tx.try_send(TelemetryCommand::Postinit).unwrap();
        }
        let principal = authorize_headers(&state, &headers, Permission::SendCommands)
            .await
            .unwrap();
        use futures::FutureExt;
        let retained = dispatch_authorized_command(
            state.clone(),
            principal,
            TelemetryCommand::Abort,
            crate::gse::automatic_generation(&state),
            None,
            None,
        )
        .now_or_never()
        .expect("manual intent retention must not await ordinary capacity");
        assert_eq!(retained.0, StatusCode::ACCEPTED);
        let epoch = state.manual_abort_intent.epoch();
        assert!(epoch > 0);
        drop(rx);
        state.last_command_ms.lock().unwrap().clear();
        let lost = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Postinit),
        )
        .await;
        assert_eq!(lost.0, StatusCode::SERVICE_UNAVAILABLE);
        // Abort owns an independent retained lane even when the ordinary worker
        // has closed. Acceptance means retained intent, not board confirmation.
        let result =
            send_command(State(state.clone()), headers, Json(TelemetryCommand::Abort)).await;
        assert_eq!(result.0, StatusCode::ACCEPTED);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    // Receipt notification is test-only and scoped to one real socket state.
    type ScopedCommandReceipt = std::sync::Mutex<Option<(usize, Arc<tokio::sync::Notify>)>>;
    static WS_COMMAND_RECEIVED: std::sync::OnceLock<ScopedCommandReceipt> = std::sync::OnceLock::new();

    pub(super) fn notify_ws_command_received(state: &Arc<AppState>) {
        let guard = WS_COMMAND_RECEIVED
            .get_or_init(Default::default)
            .lock()
            .unwrap();
        if let Some((owner, notify)) = guard.as_ref()
            && *owner == Arc::as_ptr(state) as usize
        {
            notify.notify_one();
        }
    }

    #[tokio::test]
    async fn shutdown_late_firmware_body_cannot_start_background_transfer() {
        use futures::FutureExt;
        let (_directory, state, _commands, mut headers) = preauth_fixture().await;
        headers.insert("x-firmware-filename", "mock.seds".parse().unwrap());
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = entered.clone();
        let body = Body::from_stream(futures::stream::poll_fn(move |context| {
            observed.store(true, std::sync::atomic::Ordering::Release);
            receiver.poll_recv(context)
        }));
        let request = start_firmware_update(
            State(state.clone()),
            headers,
            Path(crate::types::Board::DaqBoard.sender_id().into()),
            body,
        );
        tokio::pin!(request);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !entered.load(std::sync::atomic::Ordering::Acquire) {
                assert!(
                    request.as_mut().now_or_never().is_none(),
                    "upload returned before held body was polled"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state.request_shutdown();
        sender
            .send(Ok(axum::body::Bytes::from_static(
                b"mock-not-a-firmware-image",
            )))
            .await
            .unwrap();
        drop(sender);
        let response = request.await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let reason = to_bytes(response.into_body(), 512).await.unwrap();
        assert_eq!(
            &reason[..],
            b"ground station is shutting down",
            "router absence must not hide a missing post-body shutdown guard"
        );
        // Rejection precedes artifact decoding/router access; no board command
        // or real firmware transfer is used by this regression.
    }

    pub(crate) async fn preauth_fixture() -> (
        TestDirectory,
        Arc<AppState>,
        tokio::sync::mpsc::Receiver<TelemetryCommand>,
        HeaderMap,
    ) {
        let root = TestDirectory::new();
        let mut state = export_state(&root.0, false).await;
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::ensure_auth_sessions_table(&db).await.unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('preauth-test','operator','session',1,1,0,9999999999999)")
            .execute(&db).await.unwrap();
        std::fs::write(
            root.0.join("auth.json"),
            serde_json::json!({"users":[{
                "username":"operator", "roles":[], "password":{"salt_b64":"","hash_b64":""},
                "permissions":{"view_data":true,"send_commands":true}
            }]})
            .to_string(),
        )
        .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.auth_db = db;
        inner.cmd_tx = tx;
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer preauth-test".parse().unwrap(),
        );
        (root, state, rx, headers)
    }

    #[tokio::test]
    async fn actual_http_blocked_sender_retains_old_origin_after_receiver_drain_and_reopen() {
        let (_directory, state, mut commands, headers) = preauth_fixture().await;
        for _ in 0..4 {
            state.cmd_tx.try_send(TelemetryCommand::Postinit).unwrap();
        }
        let generation = crate::gse::automatic_generation(&state);
        let request = send_command(
            State(state.clone()),
            headers,
            Json(TelemetryCommand::Postinit),
        );
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), request.as_mut())
                .await
                .is_err()
        );
        assert!(
            state
                .last_command_ms
                .lock()
                .unwrap()
                .contains_key("dedup:Postinit"),
            "actual auth/policy/dedup reached before full-channel await"
        );
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        let token = crate::operator_queue::Token::capture(&state, 77);
        let mut admin = state.operator_queue.take_receiver().unwrap();
        let receipt = state.operator_queue.request(token).unwrap();
        crate::operator_queue::serve(&state, &mut commands, admin.recv().await.unwrap());
        assert_eq!(receipt.await.unwrap(), Ok(4));
        // Model only the verified epoch/reopen boundary, never a reset API.
        state
            .automatic_generation
            .store(generation + 1, std::sync::atomic::Ordering::Release);
        state
            .operator_admission_barrier
            .store(0, std::sync::atomic::Ordering::Release);
        assert_eq!(request.await.0, StatusCode::OK);
        let late = commands.recv().await.unwrap();
        assert_origin_rejected(&state, late, generation);
        assert!(commands.try_recv().is_err());
    }

    fn assert_origin_rejected(state: &AppState, queued: TelemetryCommand, origin: u64) {
        let TelemetryCommand::QueuedOperator {
            generation,
            command,
            ..
        } = queued
        else {
            panic!("expected existing operator envelope");
        };
        assert!(matches!(*command, TelemetryCommand::Postinit));
        assert!(
            crate::gse::claim_operator_dispatch(state, generation).is_none(),
            "pre-reset origin must never acquire dispatch after recovery"
        );
        assert_eq!(
            generation, origin,
            "authentication must not refresh a request's generation"
        );
    }

    #[tokio::test]
    async fn preauth_http_request_retains_origin_across_sqlite_wait() {
        use futures::FutureExt;
        let (_root, state, mut rx, headers) = preauth_fixture().await;
        let held = state.auth_db.acquire().await.unwrap();
        assert_eq!(state.auth_db.size(), 1);
        assert_eq!(state.auth_db.num_idle(), 0);
        let origin = crate::gse::automatic_generation(&state);
        let request = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Postinit),
        );
        tokio::pin!(request);
        assert!(
            request.as_mut().now_or_never().is_none(),
            "actual SQLite authorization must block"
        );
        state
            .automatic_generation
            .store(origin + 1, std::sync::atomic::Ordering::Release);
        drop(held);
        let stale = request.await;
        if let Ok(queued) = rx.try_recv() {
            // Predecessor runtime preserves the old envelope but still records
            // dedup: exercise its actual owner rejection before the next check.
            assert_origin_rejected(&state, queued, origin);
        }
        let fresh = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Postinit),
        )
        .await;
        let queued = rx
            .try_recv()
            .expect("stale authorization must not suppress the identical fresh command");
        let TelemetryCommand::QueuedOperator { generation, .. } = queued else {
            panic!("missing positive envelope");
        };
        assert!(crate::gse::claim_operator_dispatch(&state, generation).is_some());
        // Check stale response only after the causal owner/dedup controls.
        assert_eq!(stale.0, StatusCode::CONFLICT);
        assert_eq!(fresh.0, StatusCode::OK);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            send_command(
                State(state.clone()),
                headers,
                Json(TelemetryCommand::Postinit)
            )
            .await
            .1,
            "duplicate ignored"
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn preauth_http_revocation_and_manual_stop_keep_authentication() {
        use futures::FutureExt;
        let (_root, state, mut rx, headers) = preauth_fixture().await;
        let mut held = state.auth_db.acquire().await.unwrap();
        let origin = crate::gse::automatic_generation(&state);
        let request = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Abort),
        );
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        state
            .automatic_generation
            .store(origin + 1, std::sync::atomic::Ordering::Release);
        sqlx::query("DELETE FROM auth_sessions")
            .execute(&mut *held)
            .await
            .unwrap();
        drop(held);
        assert_eq!(request.await.0, StatusCode::UNAUTHORIZED);
        assert!(state.manual_abort_intent.pending_epoch().is_none());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            send_command(
                State(state.clone()),
                HeaderMap::new(),
                Json(TelemetryCommand::Abort)
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('preauth-test','operator','session',1,1,0,9999999999999)")
            .execute(&state.auth_db).await.unwrap();
        let held = state.auth_db.acquire().await.unwrap();
        let stop = send_command(State(state.clone()), headers, Json(TelemetryCommand::Abort));
        tokio::pin!(stop);
        assert!(stop.as_mut().now_or_never().is_none());
        state
            .automatic_generation
            .store(origin + 2, std::sync::atomic::Ordering::Release);
        drop(held);
        assert_eq!(stop.await.0, StatusCode::ACCEPTED);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn shutdown_ingress_idle_ws_closes_clean_and_late_frames_cannot_retain_stop() {
        let (_root, state, _commands, _headers) = preauth_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=preauth-test"))
                .await
                .unwrap();
        state.request_shutdown();
        assert!(
            state
                .control_ingress
                .wait_terminal(StdDuration::from_secs(2))
                .await,
            "healthy idle UI connection prevented a proven terminal receipt"
        );
        let _ = socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Abort"}"#.into(),
            ))
            .await;
        tokio::task::yield_now().await;
        assert_eq!(state.manual_abort_intent.epoch(), 0);
        let response = send_command(
            State(state.clone()),
            HeaderMap::new(),
            Json(TelemetryCommand::Abort),
        )
        .await;
        assert_eq!(response.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state.manual_abort_intent.epoch(), 0);
        server.abort();
    }
    #[tokio::test]
    async fn shutdown_ingress_held_ws_auth_finishes_existing_manual_stop_but_drops_late_frame() {
        let (_root, state, mut commands, _headers) = preauth_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=preauth-test"))
                .await
                .unwrap();
        let received = Arc::new(tokio::sync::Notify::new());
        *WS_COMMAND_RECEIVED
            .get_or_init(Default::default)
            .lock()
            .unwrap() = Some((Arc::as_ptr(&state) as usize, received.clone()));
        let held = state.auth_db.acquire().await.unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Abort"}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(2), received.notified())
            .await
            .unwrap();
        state.request_shutdown();
        assert!(
            !state
                .control_ingress
                .wait_terminal(StdDuration::from_millis(5))
                .await
        );
        assert_eq!(
            state.manual_abort_intent.epoch(),
            0,
            "authentication wait was bypassed"
        );
        let _ = socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Abort"}"#.into(),
            ))
            .await;
        drop(held);
        assert!(
            !state
                .control_ingress
                .wait_terminal(StdDuration::from_secs(2))
                .await,
            "in-flight-at-seal control was falsely labeled safe before SDK drain"
        );
        assert_eq!(
            state.manual_abort_intent.epoch(),
            1,
            "existing authorized Stop was cancelled or a post-seal frame was accepted"
        );
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(
            commands.try_recv().is_err(),
            "manual Stop entered the ordinary queue"
        );
        server.abort();
    }
    #[tokio::test]
    async fn shutdown_ingress_held_http_stop_finishes_without_reopening_new_requests() {
        use futures::FutureExt;
        let (_root, state, mut commands, headers) = preauth_fixture().await;
        let held = state.auth_db.acquire().await.unwrap();
        let request = send_command(
            State(state.clone()),
            headers.clone(),
            Json(TelemetryCommand::Abort),
        );
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        state.request_shutdown();
        drop(held);
        assert_eq!(request.await.0, StatusCode::ACCEPTED);
        assert_eq!(state.manual_abort_intent.epoch(), 1);
        assert!(
            !state
                .control_ingress
                .wait_terminal(StdDuration::from_millis(5))
                .await
        );
        assert_eq!(
            send_command(State(state.clone()), headers, Json(TelemetryCommand::Abort))
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(state.manual_abort_intent.epoch(), 1);
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn preauth_websocket_frame_retains_origin_and_stop_is_independent() {
        use futures::SinkExt;
        let (_root, state, mut rx, _headers) = preauth_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new()
            .route("/ws", axum::routing::get(ws_handler))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=preauth-test"))
                .await
                .unwrap();
        let received = Arc::new(tokio::sync::Notify::new());
        *WS_COMMAND_RECEIVED
            .get_or_init(Default::default)
            .lock()
            .unwrap() = Some((Arc::as_ptr(&state) as usize, received.clone()));
        let origin = crate::gse::automatic_generation(&state);
        let mut stale_notifications = state.notifications_tx.subscribe();
        let held = state.auth_db.acquire().await.unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Postinit"}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(3), received.notified())
            .await
            .unwrap();
        // Current-thread runtime: receiver runs through its notification into the
        // actual SQLite wait before this test can resume; the sole DB owner is held.
        state
            .automatic_generation
            .store(origin + 1, std::sync::atomic::Ordering::Release);
        drop(held);
        tokio::select! {
            queued = rx.recv() => {
                // Predecessor runtime still writes dedup despite this correct
                // old-envelope owner rejection. The identical fresh frame below
                // must be admitted without clearing that map in the test.
                assert_origin_rejected(&state, queued.unwrap(), origin);
            }
            notices = stale_notifications.recv() => {
                assert!(notices.unwrap().iter().any(|notice| notice.message == crate::operator_queue::manual_origin::Rejection::ResetChanged.message()));
            }
            _ = tokio::time::sleep(StdDuration::from_secs(3)) => panic!("stale frame did not reach ordinary admission"),
        }
        assert!(rx.try_recv().is_err());
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Postinit"}"#.into(),
            ))
            .await
            .unwrap();
        let TelemetryCommand::QueuedOperator { generation, .. } =
            tokio::time::timeout(StdDuration::from_secs(3), rx.recv())
                .await
                .expect("stale authorization must not suppress the identical fresh frame")
                .unwrap()
        else {
            panic!("missing positive envelope");
        };
        assert!(crate::gse::claim_operator_dispatch(&state, generation).is_some());
        received.notified().await;
        let held = state.auth_db.acquire().await.unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Abort"}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(3), received.notified())
            .await
            .unwrap();
        state
            .automatic_generation
            .store(origin + 2, std::sync::atomic::Ordering::Release);
        let mut notifications = state.notifications_tx.subscribe();
        // Revoke while authentication is genuinely held: even emergency
        // retention requires a currently authorized session.
        let mut held = held;
        sqlx::query("DELETE FROM auth_sessions")
            .execute(&mut *held)
            .await
            .unwrap();
        drop(held);
        tokio::time::timeout(StdDuration::from_secs(3), notifications.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(state.manual_abort_intent.pending_epoch().is_none());
        assert!(rx.try_recv().is_err());
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('preauth-test','operator','session',1,1,0,9999999999999)")
            .execute(&state.auth_db).await.unwrap();
        let held = state.auth_db.acquire().await.unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Abort"}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(3), received.notified())
            .await
            .unwrap();
        state
            .automatic_generation
            .store(origin + 3, std::sync::atomic::Ordering::Release);
        drop(held);
        // Sending a following ordinary command gives a receiver-order completion
        // barrier after Abort retention without timing sleeps.
        state.last_command_ms.lock().unwrap().clear();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"Postinit"}"#.into(),
            ))
            .await
            .unwrap();
        let _ = tokio::time::timeout(StdDuration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        *WS_COMMAND_RECEIVED.get().unwrap().lock().unwrap() = None;
        socket.close(None).await.unwrap();
        server.abort();
    }
}

#[cfg(test)]
pub(crate) mod clock_worker_tests {
    use super::*;
    use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
    use std::sync::{Condvar, Mutex, OnceLock};

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Phase {
        BeforeClaim,
        BeforeMutation,
        AfterMutation,
        Unpaused,
    }
    struct Pause {
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        changed: Condvar,
    }
    impl Pause {
        fn new() -> Self {
            Self {
                entered: tokio::sync::Notify::new(),
                released: Mutex::new(false),
                changed: Condvar::new(),
            }
        }
        fn block(&self) {
            self.entered.notify_one();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.changed.wait(released).unwrap();
            }
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.changed.notify_all();
        }
        async fn entered(&self) {
            tokio::time::timeout(StdDuration::from_secs(3), self.entered.notified())
                .await
                .expect("actual worker must reach its pause");
        }
    }
    struct Probe {
        phase: Phase,
        pause: Pause,
        completed: tokio::sync::Notify,
    }
    type WorkerProbe = Mutex<Option<(usize, Arc<Probe>)>>;
    type WriterPause = Mutex<Option<(PathBuf, Arc<Pause>)>>;
    static WORKER: OnceLock<WorkerProbe> = OnceLock::new();
    static WRITER: OnceLock<WriterPause> = OnceLock::new();
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    pub(crate) struct Completion(Arc<Probe>);
    impl Drop for Completion {
        fn drop(&mut self) {
            self.0.completed.notify_one();
        }
    }
    fn probe(state: &AppState) -> Option<Arc<Probe>> {
        WORKER
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(key, _)| *key == state as *const AppState as usize)
            .map(|(_, p)| p.clone())
    }
    pub(crate) fn completion(state: &AppState) -> Option<Completion> {
        probe(state).map(Completion)
    }
    pub(crate) fn pause_worker(state: &AppState, phase: Phase) {
        if let Some(p) = probe(state)
            && p.phase == phase
        {
            p.pause.block();
        }
    }
    pub(crate) fn pause_journal_before_save(path: &std::path::Path) {
        let p = WRITER
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(key, _)| key == path)
            .map(|(_, p)| p.clone());
        if let Some(p) = p {
            p.block();
        }
    }
    struct WorkerGuard {
        key: usize,
        probe: Arc<Probe>,
    }
    impl WorkerGuard {
        fn install(state: &Arc<AppState>, phase: Phase) -> Self {
            let key = Arc::as_ptr(state) as usize;
            let probe = Arc::new(Probe {
                phase,
                pause: Pause::new(),
                completed: tokio::sync::Notify::new(),
            });
            *WORKER.get_or_init(Default::default).lock().unwrap() = Some((key, probe.clone()));
            Self { key, probe }
        }
        async fn completed(&self) {
            tokio::time::timeout(StdDuration::from_secs(3), self.probe.completed.notified())
                .await
                .expect("actual blocking closure must complete");
        }
    }
    impl Drop for WorkerGuard {
        fn drop(&mut self) {
            self.probe.pause.release();
            let mut current = WORKER.get().unwrap().lock().unwrap();
            if current.as_ref().is_some_and(|(key, _)| *key == self.key) {
                *current = None;
            }
        }
    }
    pub(crate) struct WriterGuard {
        path: PathBuf,
        pause: Arc<Pause>,
    }
    impl WriterGuard {
        pub(crate) fn install(path: PathBuf) -> Self {
            let pause = Arc::new(Pause::new());
            *WRITER.get_or_init(Default::default).lock().unwrap() =
                Some((path.clone(), pause.clone()));
            Self { path, pause }
        }
    }
    impl WriterGuard {
        pub(crate) async fn entered(&self) {
            self.pause.entered().await;
        }
        pub(crate) fn release(&self) {
            self.pause.release();
        }
    }
    impl Drop for WriterGuard {
        fn drop(&mut self) {
            self.pause.release();
            let mut current = WRITER.get().unwrap().lock().unwrap();
            if current.as_ref().is_some_and(|(path, _)| *path == self.path) {
                *current = None;
            }
        }
    }
    async fn fixture() -> (
        crate::recording_export::tests::TestDirectory,
        Arc<AppState>,
        HeaderMap,
        PathBuf,
    ) {
        let (root, state, _rx, headers) = command_dispatch_tests::preauth_fixture().await;
        let path = root.0.join("actual-clock.json");
        let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
        *state.launch_history.lock().unwrap() =
            crate::launch_history::Store::open(path.clone(), false, None, now, 0).unwrap();
        state.set_launch_clock(LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some(now - 1_000),
            duration_ms: None,
        });
        flush(&state).await;
        (root, state, headers, path)
    }
    async fn flush(state: &AppState) {
        let waiter = state.launch_history.lock().unwrap().waiter();
        crate::launch_history::flush(waiter).await.unwrap();
    }
    fn http(
        state: Arc<AppState>,
        headers: HeaderMap,
    ) -> tokio::task::JoinHandle<(StatusCode, String)> {
        tokio::spawn(async move {
            send_command(State(state), headers, Json(TelemetryCommand::ResetTClock)).await
        })
    }
    async fn manual_stop(state: &Arc<AppState>, headers: HeaderMap) {
        let result = tokio::time::timeout(
            StdDuration::from_secs(3),
            send_command(State(state.clone()), headers, Json(TelemetryCommand::Abort)),
        )
        .await
        .unwrap();
        assert_eq!(result.0, StatusCode::ACCEPTED);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    #[tokio::test]
    async fn clock_worker_cancelled_http_keeps_actual_mutation_owned() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let before = state.launch_history.lock().unwrap().snapshot();
        let disk_before = std::fs::read(&path).unwrap();
        let origin = crate::gse::automatic_generation(&state);
        let guard = WorkerGuard::install(&state, Phase::BeforeMutation);
        let request = http(state.clone(), headers.clone());
        guard.probe.pause.entered().await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let inflight = state.operator_dispatch_inflight.load(Ordering::Acquire);
        let boundary = crate::gse::reset_generation::invalidate_capture_epoch(&state, origin);
        manual_stop(&state, headers).await;
        guard.probe.pause.release();
        guard.completed().await;
        flush(&state).await;
        let after = state.launch_history.lock().unwrap().snapshot();
        let disk_after = std::fs::read(&path).unwrap();
        eprintln!(
            "actual cancelled HTTP: inflight={inflight}, epoch_advanced={}, RAM_changed={}, journal_changed={}",
            boundary.is_ok(),
            after != before,
            disk_after != disk_before
        );
        assert_eq!(after.clock, LaunchClockMsg::idle());
        assert_ne!(
            disk_after, disk_before,
            "actual accepted worker must complete its journal write"
        );
        assert!(
            boundary.is_err(),
            "cancelled HTTP must not release the actual mutating worker's ownership"
        );
        assert_eq!(inflight, 1);
        assert!(crate::gse::reset_generation::invalidate_capture_epoch(&state, origin).is_ok());
    }
    #[tokio::test]
    async fn clock_worker_cancelled_http_keeps_actual_journal_flush_owned() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let origin = crate::gse::automatic_generation(&state);
        let worker = WorkerGuard::install(&state, Phase::AfterMutation);
        let writer = WriterGuard::install(path);
        let request = http(state.clone(), headers.clone());
        writer.pause.entered().await;
        // The nested state-clock lease must have returned before asserting the
        // outer worker's exact ownership during its separate durability wait.
        worker.probe.pause.entered().await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        let inflight = state.operator_dispatch_inflight.load(Ordering::Acquire);
        let boundary = crate::gse::reset_generation::invalidate_capture_epoch(&state, origin);
        manual_stop(&state, headers).await;
        worker.probe.pause.release();
        writer.pause.release();
        worker.completed().await;
        flush(&state).await;
        eprintln!(
            "actual cancelled HTTP journal flush: inflight={inflight}, epoch_advanced={}",
            boundary.is_ok()
        );
        assert!(
            boundary.is_err(),
            "journal ticket still writing must retain the worker claim through HTTP cancellation"
        );
        assert_eq!(inflight, 1);
        assert!(crate::gse::reset_generation::invalidate_capture_epoch(&state, origin).is_ok());
    }
    #[tokio::test]
    async fn clock_worker_websocket_spawn_before_epoch_cannot_write_late() {
        let _serial = SERIAL.lock().await;
        let (_root, state, _headers, path) = fixture().await;
        let before = state.launch_history.lock().unwrap().snapshot();
        let disk_before = std::fs::read(&path).unwrap();
        let origin = crate::gse::automatic_generation(&state);
        let guard = WorkerGuard::install(&state, Phase::BeforeClaim);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=preauth-test"))
                .await
                .unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"ResetTClock"}"#.into(),
            ))
            .await
            .unwrap();
        guard.probe.pause.entered().await;
        assert!(crate::gse::reset_generation::invalidate_capture_epoch(&state, origin).is_ok());
        // Actual WS task has already spawned actual blocking work. Closing the
        // socket must not make that old origin acquire fresh authority later.
        socket.close(None).await.unwrap();
        guard.probe.pause.release();
        guard.completed().await;
        flush(&state).await;
        let after = state.launch_history.lock().unwrap().snapshot();
        let disk_after = std::fs::read(&path).unwrap();
        eprintln!(
            "actual detached WS: RAM_changed={}, journal_changed={}",
            after != before,
            disk_after != disk_before
        );
        server.abort();
        assert!(
            after == before && disk_after == disk_before,
            "spawned old-origin WS reset must not mutate RAM or journal after the epoch boundary"
        );
    }
    #[tokio::test]
    async fn clock_worker_http_spawn_before_epoch_returns409_without_mutation() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let before = state.launch_history.lock().unwrap().snapshot();
        let disk_before = std::fs::read(&path).unwrap();
        let origin = crate::gse::automatic_generation(&state);
        let worker = WorkerGuard::install(&state, Phase::BeforeClaim);
        let request = http(state.clone(), headers);
        worker.probe.pause.entered().await;
        assert!(crate::gse::reset_generation::invalidate_capture_epoch(&state, origin).is_ok());
        worker.probe.pause.release();
        worker.completed().await;
        assert_eq!(request.await.unwrap().0, StatusCode::CONFLICT);
        flush(&state).await;
        assert_eq!(state.launch_history.lock().unwrap().snapshot(), before);
        assert_eq!(std::fs::read(&path).unwrap(), disk_before);
    }

    #[tokio::test]
    async fn clock_worker_current_http_and_websocket_remain_durable_and_authorized() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let before = state.launch_history.lock().unwrap().snapshot();
        let disk_before = std::fs::read(&path).unwrap();
        assert_eq!(
            send_command(
                State(state.clone()),
                HeaderMap::new(),
                Json(TelemetryCommand::ResetTClock)
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(state.launch_history.lock().unwrap().snapshot(), before);
        assert_eq!(std::fs::read(&path).unwrap(), disk_before);
        assert_eq!(
            http(state.clone(), headers).await.unwrap().0,
            StatusCode::OK
        );
        let clock = state.launch_history.lock().unwrap().snapshot();
        assert_eq!(clock.clock, LaunchClockMsg::idle());
        assert_eq!(clock.revision, before.revision + 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()["current"],
            serde_json::to_value(&clock).unwrap()
        );
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);

        let (_root_ws, ws_state, _headers, ws_path) = fixture().await;
        let before = ws_state.launch_history.lock().unwrap().snapshot();
        let worker = WorkerGuard::install(&ws_state, Phase::Unpaused);
        let mut notices = ws_state.notifications_tx.subscribe();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(ws_state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=preauth-test"))
                .await
                .unwrap();
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"ResetTClock"}"#.into(),
            ))
            .await
            .unwrap();
        worker.completed().await;
        let saved = tokio::time::timeout(StdDuration::from_secs(3), notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            saved
                .iter()
                .any(|n| n.message == "Primary T clock reset saved")
        );
        let clock = ws_state.launch_history.lock().unwrap().snapshot();
        assert_eq!(clock.clock, LaunchClockMsg::idle());
        assert_eq!(clock.revision, before.revision + 1);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(ws_path).unwrap()).unwrap()
                ["current"],
            serde_json::to_value(&clock).unwrap()
        );
        assert_eq!(
            ws_state.operator_dispatch_inflight.load(Ordering::Acquire),
            0
        );
        socket.close(None).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn clock_worker_writer_timeout_is507_and_not_writer_quiescence() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let disk_before = std::fs::read(&path).unwrap();
        let worker = WorkerGuard::install(&state, Phase::AfterMutation);
        let writer = WriterGuard::install(path.clone());
        let request = http(state.clone(), headers.clone());
        writer.pause.entered().await;
        // The nested state-clock lease must have returned before asserting the
        // outer worker's exact ownership during its separate durability wait.
        worker.probe.pause.entered().await;
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        manual_stop(&state, headers).await;
        worker.probe.pause.release();
        // Exercise the production ten-second wait deadline unchanged. The
        // separate journal thread stays paused after this HTTP failure.
        let response = tokio::time::timeout(StdDuration::from_secs(15), request)
            .await
            .unwrap()
            .unwrap();
        worker.completed().await;
        assert_eq!(
            response,
            (
                StatusCode::INSUFFICIENT_STORAGE,
                "T clock reset was not saved".to_string()
            )
        );
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        assert!(
            !*writer.pause.released.lock().unwrap(),
            "the actual writer is still blocked after timeout"
        );
        assert_eq!(std::fs::read(&path).unwrap(), disk_before);
        assert_eq!(
            state.launch_history.lock().unwrap().snapshot().clock,
            LaunchClockMsg::idle()
        );
        eprintln!(
            "actual timeout: HTTP507, claim_released=true, actual_writer_still_paused=true, journal_unchanged=true; NOT quiescent"
        );
        // No epoch advance is attempted: the full coordinator must inspect
        // its journal prefix rather than use a timeout/zero owner count as proof.
        writer.pause.release();
        flush(&state).await;
        assert_ne!(std::fs::read(path).unwrap(), disk_before);
    }

    #[tokio::test]
    async fn recovery_admission_journal_blocks_epoch_until_actual_prefix_and_preserves_manual_stop()
    {
        use crate::gse::reset_generation::{
            advance_with_clock_fence, close_operator_admission,
            reopen_operator_admission_and_journal,
        };
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, _path) = fixture().await;
        let generation = crate::gse::automatic_generation(&state);
        let inflight = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
        close_operator_admission(&state, generation, 71).unwrap();
        assert!(close_operator_admission(&state, generation, 72).is_err());
        close_operator_admission(&state, generation, 71).unwrap();
        assert!(crate::gse::claim_operator_dispatch(&state, generation).is_none());
        assert_eq!(
            crate::gse::record_operator_command(
                &state,
                generation,
                &TelemetryCommand::Postinit,
                10_000,
                500
            ),
            crate::gse::OperatorCommandAdmission::Stale
        );
        assert!(
            crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::Nitrogen)
                .is_none()
        );
        assert!(matches!(
            crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::Abort),
            Some(TelemetryCommand::Abort)
        ));
        // The real authenticated HTTP clock mutation is also refused while closed.
        let rejected = http(state.clone(), headers.clone()).await.unwrap();
        manual_stop(&state, headers).await;
        assert_eq!(rejected.0, StatusCode::CONFLICT);
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let fence = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(71)
            .unwrap();
        assert!(
            advance_with_clock_fence(&state, generation, 71, &fence).is_err(),
            "old unstaged prefix cannot publish"
        );
        let writer = WriterGuard::install(_path.clone());
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&fence)
            .unwrap();
        writer.pause.entered().await;
        assert!(
            advance_with_clock_fence(&state, generation, 71, &staged).is_err(),
            "pending real writer cannot publish"
        );
        assert!(reopen_operator_admission_and_journal(&state, generation, 71, &staged).is_err());
        writer.pause.release();
        flush(&state).await;
        assert!(
            advance_with_clock_fence(&state, generation, 71, &staged).is_err(),
            "inflight owner still blocks publication"
        );
        drop(inflight);
        assert!(advance_with_clock_fence(&state, generation, 72, &staged).is_err());
        assert_eq!(
            advance_with_clock_fence(&state, generation, 71, &staged).unwrap(),
            generation + 1
        );
        assert!(crate::gse::claim_operator_dispatch(&state, generation + 1).is_none());
        assert!(crate::gse::capture_operator_origin(&state).is_none());
        assert!(advance_with_clock_fence(&state, generation, 71, &staged).is_err());
        assert!(reopen_operator_admission_and_journal(&state, generation, 72, &staged).is_err());
        reopen_operator_admission_and_journal(&state, generation, 71, &staged).unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        assert!(crate::gse::claim_operator_dispatch(&state, generation).is_none());
        assert!(crate::gse::claim_operator_dispatch(&state, generation + 1).is_some());
        assert_eq!(
            crate::gse::record_operator_command(
                &state,
                generation,
                &TelemetryCommand::Postinit,
                10_001,
                500
            ),
            crate::gse::OperatorCommandAdmission::Stale
        );
        assert_eq!(
            crate::gse::record_operator_command(
                &state,
                generation + 1,
                &TelemetryCommand::Postinit,
                10_002,
                500
            ),
            crate::gse::OperatorCommandAdmission::Fresh
        );
        assert!(reopen_operator_admission_and_journal(&state, generation, 71, &staged).is_err());
    }

    #[tokio::test]
    async fn recovery_owner_transfer_preserves_pending_writer_and_revokes_every_old_ticket() {
        use crate::gse::reset_generation::{
            advance_with_clock_fence, close_operator_admission,
            reopen_operator_admission_and_journal, transfer_closed_recovery_owner,
        };
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        let generation = crate::gse::automatic_generation(&state);
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let before = state.launch_history.lock().unwrap().snapshot();
        let inflight = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
        close_operator_admission(&state, generation, 501).unwrap();
        let first = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(501)
            .unwrap();
        let writer = WriterGuard::install(path.clone());
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&first)
            .unwrap();
        writer.pause.entered().await;
        let disk = std::fs::read(&path).unwrap();
        assert!(transfer_closed_recovery_owner(&state, generation, 501, 502, &staged).is_err());
        drop(inflight);
        for (expected, old, new) in [
            (generation + 1, 501, 502),
            (generation, 500, 502),
            (generation, 501, 0),
            (generation, 501, u64::MAX),
            (generation, 501, 501),
        ] {
            assert!(transfer_closed_recovery_owner(&state, expected, old, new, &staged).is_err());
        }
        let transferred =
            transfer_closed_recovery_owner(&state, generation, 501, 502, &staged).unwrap();
        assert!(crate::gse::capture_operator_origin(&state).is_none());
        assert!(http(state.clone(), headers.clone()).await.unwrap().0 == StatusCode::CONFLICT);
        assert_eq!(std::fs::read(&path).unwrap(), disk);
        assert!(
            !state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&transferred)
                .unwrap()
        );
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&staged)
                .is_err()
        );
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .retry_recovery_save(&staged)
                .is_err()
        );
        assert!(transfer_closed_recovery_owner(&state, generation, 501, 503, &staged).is_err());
        assert!(advance_with_clock_fence(&state, generation, 502, &transferred).is_err());
        assert!(reopen_operator_admission_and_journal(&state, generation, 501, &staged).is_err());
        writer.pause.release();
        flush(&state).await;
        assert_eq!(
            advance_with_clock_fence(&state, generation, 502, &transferred).unwrap(),
            generation + 1
        );
        // A later terminated supervisor may have already advanced the host
        // generation. Its replacement must use the current generation and
        // retain the same staged clock instead of incrementing it twice.
        let newest =
            transfer_closed_recovery_owner(&state, generation + 1, 502, 503, &transferred).unwrap();
        assert!(
            reopen_operator_admission_and_journal(&state, generation, 502, &transferred).is_err()
        );
        reopen_operator_admission_and_journal(&state, generation, 503, &newest).unwrap();
        let after = state.launch_history.lock().unwrap().snapshot();
        assert_eq!(after.revision, before.revision + 1);
        assert_eq!(after.clock, before.clock);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        assert!(crate::gse::claim_operator_dispatch(&state, generation).is_none());
        assert!(crate::gse::claim_operator_dispatch(&state, generation + 1).is_some());
        assert!(transfer_closed_recovery_owner(&state, generation + 1, 503, 504, &newest).is_err());
    }

    #[tokio::test]
    async fn recovery_owner_transfer_keeps_terminal_io_failure_closed_until_explicit_retry() {
        use crate::gse::reset_generation::{
            advance_with_clock_fence, close_operator_admission,
            reopen_operator_admission_and_journal, transfer_closed_recovery_owner,
        };
        let _serial = SERIAL.lock().await;
        let (_root, state, _headers, path) = fixture().await;
        let generation = crate::gse::automatic_generation(&state);
        close_operator_admission(&state, generation, 601).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("sentinel"), b"preserve").unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .inhibit_automatic_actions()
                .is_err()
        );
        let first = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(601)
            .unwrap();
        let transferred =
            transfer_closed_recovery_owner(&state, generation, 601, 602, &first).unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&transferred)
                .is_err()
        );
        assert!(advance_with_clock_fence(&state, generation, 602, &transferred).is_err());
        assert!(
            reopen_operator_admission_and_journal(&state, generation, 602, &transferred).is_err()
        );
        assert_eq!(std::fs::read(path.join("sentinel")).unwrap(), b"preserve");
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .retry_recovery_save(&first)
                .is_err()
        );
        // External fixture repair is deliberate; the implementation never
        // removes/rewrites an unreadable target just to make recovery succeed.
        std::fs::remove_dir_all(&path).unwrap();
        let retried = state
            .launch_history
            .lock()
            .unwrap()
            .retry_recovery_save(&transferred)
            .unwrap();
        flush(&state).await;
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&retried)
            .unwrap();
        flush(&state).await;
        advance_with_clock_fence(&state, generation, 602, &staged).unwrap();
        // Shutdown always defeats the supervisor, including a replacement.
        state.launch_history.lock().unwrap().close_and_waiter();
        assert!(transfer_closed_recovery_owner(&state, generation + 1, 602, 603, &staged).is_err());
        assert!(reopen_operator_admission_and_journal(&state, generation, 602, &staged).is_err());
        assert!(crate::gse::capture_operator_origin(&state).is_none());
    }

    #[tokio::test]
    async fn recovery_remote_clock_keeps_receive_origin_and_actual_worker_lease_through_save() {
        use crate::gse::reset_generation::{
            advance_with_clock_fence, close_operator_admission,
            reopen_operator_admission_and_journal,
        };
        let _serial = SERIAL.lock().await;
        let (_root, state, _headers, path) = fixture().await;
        let original = crate::gse::capture_operator_origin(&state).unwrap();
        close_operator_admission(&state, original, 91).unwrap();
        let closed_origin = crate::gse::capture_operator_origin(&state);
        assert!(closed_origin.is_none());
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let fence = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(91)
            .unwrap();
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&fence)
            .unwrap();
        flush(&state).await;
        advance_with_clock_fence(&state, original, 91, &staged).unwrap();
        reopen_operator_admission_and_journal(&state, original, 91, &staged).unwrap();
        let expected = state.launch_history.lock().unwrap().snapshot();
        let disk = std::fs::read(&path).unwrap();
        // Even a request carrying the newly visible record cannot become a
        // new request if it was received while admission was closed.
        for rejected_origin in [closed_origin, Some(original)] {
            assert!(
                crate::launch_clock_sync::reset_primary_for_origin(
                    &state,
                    rejected_origin,
                    &expected
                )
                .is_err()
            );
            assert_eq!(state.launch_history.lock().unwrap().snapshot(), expected);
            assert_eq!(std::fs::read(&path).unwrap(), disk);
        }
        let fresh = crate::gse::capture_operator_origin(&state);
        let worker = WorkerGuard::install(&state, Phase::AfterMutation);
        let writer = WriterGuard::install(path.clone());
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let work = state.clone();
        let actual = tokio::task::spawn_blocking(move || {
            let result =
                crate::launch_clock_sync::reset_primary_for_origin(&work, fresh, &expected);
            let _ = done_tx.send(result);
        });
        writer.pause.entered().await;
        // The nested state-clock lease must have returned before asserting the
        // outer worker's exact ownership during its separate durability wait.
        worker.probe.pause.entered().await;
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        actual.abort();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        let current_generation = crate::gse::automatic_generation(&state);
        assert!(
            crate::gse::reset_generation::invalidate_capture_epoch(&state, current_generation)
                .is_err()
        );
        assert_eq!(crate::gse::automatic_generation(&state), current_generation);
        assert_eq!(std::fs::read(&path).unwrap(), disk);
        worker.probe.pause.release();
        writer.pause.release();
        tokio::time::timeout(StdDuration::from_secs(3), done_rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        assert_ne!(std::fs::read(&path).unwrap(), disk);
    }

    #[tokio::test]
    async fn recovery_journal_timeout_never_proves_quiescence_and_closed_store_blocks_late_reset() {
        let _serial = SERIAL.lock().await;
        let (_root, state, headers, path) = fixture().await;
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let worker = WorkerGuard::install(&state, Phase::Unpaused);
        let writer = WriterGuard::install(path.clone());
        let request = http(state.clone(), headers.clone());
        writer.pause.entered().await;
        let response = tokio::time::timeout(StdDuration::from_secs(15), request)
            .await
            .unwrap()
            .unwrap();
        worker.completed().await;
        assert_eq!(response.0, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        let disk = std::fs::read(&path).unwrap();
        let fence = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(7)
            .unwrap();
        assert!(
            !state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&fence)
                .unwrap()
        );
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .reopen_after_recovery(&fence)
                .is_err()
        );
        let clock = state.launch_history.lock().unwrap().snapshot();
        let late = tokio::time::timeout(StdDuration::from_secs(3), http(state.clone(), headers))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(late.0, StatusCode::CONFLICT);
        assert_eq!(state.launch_history.lock().unwrap().snapshot(), clock);
        assert_eq!(std::fs::read(&path).unwrap(), disk);
        writer.pause.release();
        flush(&state).await;
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&fence)
                .unwrap()
        );
        assert_ne!(std::fs::read(&path).unwrap(), disk);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .reopen_after_recovery(&fence)
                .is_err()
        );
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&fence)
            .unwrap();
        flush(&state).await;
        state
            .launch_history
            .lock()
            .unwrap()
            .reopen_after_recovery(&staged)
            .unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        eprintln!(
            "actual timed-out worker: claim0 whilewriterpaused => fencePending; actual lateHTTP409 noRAM/noJournal; release+realflush => exactprefixDurable; inhibitionretained"
        );
    }

    #[tokio::test]
    async fn recovery_journal_wrong_owner_and_shutdown_cannot_reopen_a_recovery_fence() {
        let _serial = SERIAL.lock().await;
        let (_root, state, _headers, _path) = fixture().await;
        let mut history = state.launch_history.lock().unwrap();
        assert!(
            history.close_for_recovery(7).is_err(),
            "inhibition is a required precondition"
        );
        history.inhibit_automatic_actions().unwrap();
        let fence = history.close_for_recovery(7).unwrap();
        assert!(history.recovery_quiescent(&fence).unwrap());
        assert!(history.close_for_recovery(8).is_err());
        assert!(history.reset_primary(123456, 0).is_err());
        history.close_and_waiter();
        assert!(history.recovery_quiescent(&fence).is_err());
        assert!(history.reopen_after_recovery(&fence).is_err());
        assert!(history.automatic_actions_inhibited());
    }

    #[tokio::test]
    async fn recovery_journal_failed_atomic_write_remains_closed_and_never_formats_the_path() {
        let _serial = SERIAL.lock().await;
        let (root, state, _headers, path) = fixture().await;
        // Occupy the exact target with a directory in this disposable
        // fixture; the actual writer must fail rather than reformat it.
        let saved = std::fs::read(&path).unwrap();
        let sibling = root.0.join("saved-before-write-failure");
        std::fs::write(&sibling, &saved).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("sentinel"), b"keep this directory").unwrap();
        let fence = {
            let mut history = state.launch_history.lock().unwrap();
        assert!(history.inhibit_automatic_actions().is_err());
        let fence = history.close_for_recovery(7).unwrap();
        assert!(history.recovery_quiescent(&fence).is_err());
        assert!(history.reopen_after_recovery(&fence).is_err());
        assert!(history.reset_primary(123456, 0).is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read(path.join("sentinel")).unwrap(),
            b"keep this directory"
        );
        assert_eq!(std::fs::read(sibling).unwrap(), saved);
            fence
        };
        // Only this explicit repair+retry of the known snapshot resumes IO.
        std::fs::remove_dir_all(&path).unwrap();
        let retried = state
            .launch_history
            .lock()
            .unwrap()
            .retry_recovery_save(&fence)
            .unwrap();
        flush(&state).await;
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&retried)
                .unwrap()
        );
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&retried)
            .unwrap();
        flush(&state).await;
        state
            .launch_history
            .lock()
            .unwrap()
            .reopen_after_recovery(&staged)
            .unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
    }

    #[tokio::test]
    async fn recovery_journal_revision_preserves_time_but_rejects_preboundary_remote_reset_contract()
     {
        let _serial = SERIAL.lock().await;
        let (_root, state, _headers, path) = fixture().await;
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let original = state.launch_history.lock().unwrap().snapshot();
        let fence = state
            .launch_history
            .lock()
            .unwrap()
            .close_for_recovery(7)
            .unwrap();
        let staged = state
            .launch_history
            .lock()
            .unwrap()
            .stage_recovery_revision(&fence)
            .unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .recovery_quiescent(&fence)
                .is_err()
        );
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .stage_recovery_revision(&staged)
                .is_err()
        );
        flush(&state).await;
        state
            .launch_history
            .lock()
            .unwrap()
            .reopen_after_recovery(&staged)
            .unwrap();
        let current = state.launch_history.lock().unwrap().snapshot();
        assert_eq!(current.clock, original.clock);
        assert_eq!(current.t0_utc_ms, original.t0_utc_ms);
        assert_eq!(current.session, original.session);
        assert_eq!(current.authority, original.authority);
        assert_eq!(current.revision, original.revision + 1);
        let disk = std::fs::read(&path).unwrap();
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .reset_expected_primary(&original, 123456, 0)
                .is_err()
        );
        assert_eq!(state.launch_history.lock().unwrap().snapshot(), current);
        assert_eq!(std::fs::read(&path).unwrap(), disk);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
    }
}

/// Query parameters accepted by the authenticated WebSocket endpoint.
#[derive(Deserialize, Default)]
struct WsAuthQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
struct AlertAckRequest {
    warning_timestamp_ms: i64,
    error_timestamp_ms: i64,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsAuthQuery>,
) -> impl IntoResponse {
    let Some(lease) = state.control_ingress.connection() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Control ingress is closed").into_response();
    };
    let _principal = match state
        .auth
        .authorize_token(&state.auth_db, query.token.as_deref(), Permission::ViewData)
        .await
    {
        Ok(principal) => principal,
        Err(err) => {
            lease.finish();
            return auth_failure_response(err);
        }
    };
    if state.control_ingress.is_closed() {
        lease.finish();
        return (StatusCode::SERVICE_UNAVAILABLE, "Control ingress is closed").into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state, query.token, lease))
}

/// Shape of commands sent from the frontend over WebSocket:
/// { "cmd": "Arm" } or { "cmd": "Disarm" }
#[derive(Deserialize)]
struct WsCommand {
    cmd: TelemetryCommand,
}

fn new_notification_events(
    history: &[PersistentNotification],
    last_id: &mut u64,
) -> Vec<PersistentNotification> {
    let mut events: Vec<_> = history
        .iter()
        .filter(|n| n.id > *last_id)
        .cloned()
        .collect();
    events.sort_by_key(|n| n.id);
    if let Some(event) = events.last() {
        *last_id = event.id;
    }
    events
}

#[cfg(test)]
mod notification_delivery_tests {
    use super::*;
    fn notice(id: u64) -> PersistentNotification {
        PersistentNotification {
            id,
            timestamp_ms: id as i64,
            message: "T clock reset".into(),
            persistent: false,
            action_label: None,
            action_cmd: None,
        }
    }
    #[test]
    fn connection_history_and_repeated_snapshots_do_not_replay_events() {
        let mut history = vec![notice(1), notice(2)];
        let mut connection_cutoff = history.last().unwrap().id;
        assert!(new_notification_events(&history, &mut connection_cutoff).is_empty());
        history.push(notice(3));
        let events = new_notification_events(&history, &mut connection_cutoff);
        assert_eq!(events.iter().map(|n| n.id).collect::<Vec<_>>(), vec![3]);
        assert!(new_notification_events(&history, &mut connection_cutoff).is_empty());
        let mut reconnect_cutoff = history.last().unwrap().id;
        assert!(new_notification_events(&history, &mut reconnect_cutoff).is_empty());
        let live = serde_json::to_value(WsOutMsg::Notification(notice(4))).unwrap();
        assert_eq!(live["ty"], "Notification");
        assert_eq!(
            serde_json::to_value(WsOutMsg::Notifications(history)).unwrap()["ty"],
            "Notifications"
        );
    }
}

/// One bounded live display frame. Each stream keeps its recent rows, so a
/// radio burst of several samples reaches the display instead of only the
/// newest one. Raw telemetry remains in DB/recent; a slow browser still cannot
/// queue seconds of frames (per-stream and stream-count caps, frame max age).
#[derive(Default)]
struct LatestWsTelemetry {
    pending: std::sync::Mutex<Option<PendingWsTelemetry>>,
    ready: tokio::sync::Notify,
}
struct PendingWsTelemetry {
    updated: Instant,
    rows: HashMap<(String, String), VecDeque<TelemetryRow>>,
}
const WS_LIVE_STREAM_CAP: usize = 1024;
/// Rows kept per stream in one unsent frame. A 250 ms frame of a 40 Hz stream
/// holds ten; anything beyond this is a slow display and keeps the newest.
const WS_LIVE_ROWS_PER_STREAM: usize = 32;
const WS_LIVE_FRAME_MAX_AGE: StdDuration = StdDuration::from_millis(250);
const WS_WRITE_TIMEOUT: StdDuration = StdDuration::from_secs(2);

impl LatestWsTelemetry {
    fn publish(&self, rows: Vec<TelemetryRow>) -> bool {
        let mut slot = self.pending.lock().unwrap();
        let coalesced = slot.is_some();
        let pending = slot.get_or_insert_with(|| PendingWsTelemetry {
            updated: Instant::now(),
            rows: HashMap::new(),
        });
        for row in rows {
            let key = (row.sender_id.clone(), row.data_type.clone());
            if !pending.rows.contains_key(&key) && pending.rows.len() >= WS_LIVE_STREAM_CAP {
                // A hostile/unbounded runtime schema cannot grow the display
                // slot forever. Retire only its oldest telemetry stream.
                if let Some(oldest) = pending
                    .rows
                    .iter()
                    .min_by_key(|(_, rows)| rows.back().map_or(i64::MIN, |row| row.timestamp_ms))
                    .map(|(key, _)| key.clone())
                {
                    pending.rows.remove(&oldest);
                }
            }
            let stream = pending.rows.entry(key).or_default();
            // Older retransmissions cannot roll a stream backward.
            if stream.back().is_some_and(|last| row.timestamp_ms < last.timestamp_ms) {
                continue;
            }
            if stream.len() >= WS_LIVE_ROWS_PER_STREAM {
                stream.pop_front();
            }
            stream.push_back(row);
        }
        pending.updated = Instant::now();
        drop(slot);
        self.ready.notify_one();
        coalesced
    }
    async fn recv(&self) -> (Instant, Vec<TelemetryRow>) {
        loop {
            self.ready.notified().await;
            let frame = self.pending.lock().unwrap().take();
            if let Some(frame) = frame {
                let mut rows: Vec<_> = frame.rows.into_values().flatten().collect();
                rows.sort_by_key(|row| row.timestamp_ms);
                return (frame.updated, rows);
            }
        }
    }
}

async fn next_ws_output(
    control: &mut mpsc::Receiver<String>,
    telemetry: &LatestWsTelemetry,
) -> Option<String> {
    loop {
        tokio::select! {
            biased;
            // Ordered warnings, confirmations and state changes never compete
            // for FIFO space with high-rate chart data.
            message = control.recv() => return message,
            (queued, rows) = telemetry.recv() => {
                if queued.elapsed() <= WS_LIVE_FRAME_MAX_AGE {
                    return Some(serde_json::to_string(&WsOutMsg::TelemetryBatch(rows)).unwrap_or_default());
                }
            }
        }
    }
}

#[cfg(test)]
mod ws_freshness_tests {
    use super::*;
    fn row(ty: &str, timestamp_ms: i64) -> TelemetryRow {
        TelemetryRow {
            timestamp_ms,
            data_type: ty.into(),
            sender_id: "DAQ".into(),
            values: vec![Some(timestamp_ms as f32)],
        }
    }
    #[tokio::test]
    async fn slow_display_coalesces_telemetry_but_preserves_ordered_controls() {
        let latest = LatestWsTelemetry::default();
        let (tx, mut rx) = mpsc::channel(8);
        tx.send("warning".into()).await.unwrap();
        tx.send("confirmation".into()).await.unwrap();
        assert!(!latest.publish(vec![row("KG1000", 0)]));
        for sequence in 1..1000 {
            assert!(latest.publish(vec![row("KG1000", sequence)]));
        }
        assert_eq!(
            next_ws_output(&mut rx, &latest).await.as_deref(),
            Some("warning")
        );
        assert_eq!(
            next_ws_output(&mut rx, &latest).await.as_deref(),
            Some("confirmation")
        );
        let frame: serde_json::Value =
            serde_json::from_str(&next_ws_output(&mut rx, &latest).await.unwrap()).unwrap();
        assert_eq!(frame["ty"], "TelemetryBatch");
        // A slow display keeps a bounded tail of each stream, newest last.
        let data = frame["data"].as_array().unwrap();
        assert_eq!(data.len(), WS_LIVE_ROWS_PER_STREAM);
        assert_eq!(data[0]["timestamp_ms"], 1000 - WS_LIVE_ROWS_PER_STREAM as i64);
        assert_eq!(data[data.len() - 1]["timestamp_ms"], 999);
        assert!(latest.pending.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn sparse_pressure_survives_loadcell_bursts_and_stream_count_is_bounded() {
        let latest = LatestWsTelemetry::default();
        latest.publish(vec![row("COMBUSTION_CHAMBER_PRESSURE", 50)]);
        for sequence in 51..1000 {
            latest.publish(vec![row("KG50", sequence), row("KG1000", sequence)]);
        }
        // Older retransmissions cannot roll a stream backward.
        latest.publish(vec![row("KG1000", 10)]);
        let (_, rows) = latest.recv().await;
        assert_eq!(rows.len(), 1 + 2 * WS_LIVE_ROWS_PER_STREAM);
        assert!(!rows.iter().any(|r| r.data_type == "KG1000" && r.timestamp_ms == 10));
        assert!(
            rows.iter()
                .any(|r| r.data_type == "COMBUSTION_CHAMBER_PRESSURE" && r.timestamp_ms == 50)
        );
        assert!(
            rows.iter()
                .any(|r| r.data_type == "KG1000" && r.timestamp_ms == 999)
        );
        for sequence in 0..2048 {
            latest.publish(vec![row(&format!("stream{sequence}"), sequence)]);
        }
        assert_eq!(latest.recv().await.1.len(), WS_LIVE_STREAM_CAP);
    }
    /// Radio links deliver several samples of one stream in one burst. Each
    /// sample must reach the display, not only the newest one in the flush.
    #[tokio::test]
    async fn burst_of_one_stream_reaches_the_display_in_order() {
        let latest = LatestWsTelemetry::default();
        let (_tx, mut rx) = mpsc::channel::<String>(1);
        latest.publish((0..5).map(|i| row("ACCEL_DATA", 1_000 + i * 200)).collect());
        let frame: serde_json::Value =
            serde_json::from_str(&next_ws_output(&mut rx, &latest).await.unwrap()).unwrap();
        let stamps = frame["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["timestamp_ms"].as_i64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(stamps, vec![1_000, 1_200, 1_400, 1_600, 1_800]);
    }

    #[tokio::test]
    async fn expired_unsent_display_frame_is_not_replayed() {
        let latest = LatestWsTelemetry::default();
        let (tx, mut rx) = mpsc::channel(1);
        latest.publish(vec![row("KG1000", 1)]);
        latest.pending.lock().unwrap().as_mut().unwrap().updated =
            Instant::now() - StdDuration::from_secs(30);
        assert!(
            tokio::time::timeout(
                StdDuration::from_millis(10),
                next_ws_output(&mut rx, &latest)
            )
            .await
            .is_err()
        );
        latest.publish(vec![row("KG1000", 2)]);
        let frame = next_ws_output(&mut rx, &latest).await.unwrap();
        assert!(frame.contains("\"timestamp_ms\":2"));
        drop(tx);
    }
}

async fn handle_ws(
    socket: WebSocket,
    state: Arc<AppState>,
    token: Option<String>,
    connection: crate::control_ingress::Lease,
) {
    // Subscribe to all three broadcast channels
    let mut telemetry_rx = state.ws_tx.subscribe();
    let mut warnings_rx = state.warnings_tx.subscribe();
    let mut errors_rx = state.errors_tx.subscribe();
    let mut board_abort_rx = state.board_abort_tx.subscribe();
    let mut state_rx = state.state_tx.subscribe();
    let mut launch_clock_rx = state.launch_clock_tx.subscribe();
    let mut board_status_rx = state.board_status_tx.subscribe();
    let mut notifications_rx = state.notifications_tx.subscribe();
    let mut messages_rx = state.messages_tx.subscribe();
    let mut action_policy_rx = state.action_policy_tx.subscribe();
    let mut fill_targets_rx = state.fill_targets_tx.subscribe();
    let mut recording_status_rx = state.recording_status_tx.subscribe();
    let mut dashboard_reset_rx = state.dashboard_reset_tx.subscribe();
    let mut alert_ack_rx = state.alert_ack_tx.subscribe();
    let mut alert_conditions_rx=state.alert_conditions.tx.subscribe();
    let mut mission_phase_rx = crate::mission_phase::subscribe();
    let mut flight_maxima_rx = crate::flight_maxima::subscribe();
    let mut station_mode_rx = crate::runtime_mode::subscribe();
    station_mode_rx.borrow_and_update();

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

    let live_telemetry = Arc::new(LatestWsTelemetry::default());
    let live_telemetry_writer = live_telemetry.clone();
    // One live display slot plus the ordered control queue. Bound a stalled
    // socket write so reconnect can reseed rather than replay an old backlog.
    let mut write_task = tokio::spawn(async move {
        while let Some(text) = next_ws_output(&mut ws_out_rx, &live_telemetry_writer).await {
            if !matches!(
                tokio::time::timeout(
                    WS_WRITE_TIMEOUT,
                    socket_sender.send(Message::Text(Utf8Bytes::from(text)))
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
    });

    // Task: collect, batch, and enqueue outbound messages.
    let send_task = async move {
        let notification_history = state_for_send.notifications_snapshot();
        let mut last_notification_id = notification_history.iter().map(|n| n.id).max().unwrap_or(0);
        let initial_notifications =
            serde_json::to_string(&WsOutMsg::Notifications(notification_history))
                .unwrap_or_default();
        if ws_out_tx.send(initial_notifications).await.is_err() {
            return;
        }
        let initial_messages =
            serde_json::to_string(&WsOutMsg::Messages(state_for_send.messages_snapshot()))
                .unwrap_or_default();
        if ws_out_tx.send(initial_messages).await.is_err() {
            return;
        }
        crate::sequences::refresh_action_policy_now(&state_for_send);
        let initial_action_policy = serde_json::to_string(&WsOutMsg::ActionPolicy(
            state_for_send.action_policy_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_action_policy).await.is_err() {
            return;
        }
        let initial_flight_state = serde_json::to_string(&WsOutMsg::FlightState(FlightStateWire::new(
            state_for_send.local_flight_state_snapshot(),
            &state_for_send,
        )))
        .unwrap_or_default();
        if ws_out_tx.send(initial_flight_state).await.is_err() {
            return;
        }
        let initial_maxima = flight_maxima_rx.borrow_and_update().clone();
        if ws_out_tx
            .send(serde_json::to_string(&WsOutMsg::FlightMaxima(initial_maxima)).unwrap_or_default())
            .await
            .is_err()
        {
            return;
        }
        let initial_phase = mission_phase_rx.borrow_and_update().clone();
        if ws_out_tx
            .send(serde_json::to_string(&WsOutMsg::MissionPhase(initial_phase)).unwrap_or_default())
            .await
            .is_err()
        {
            return;
        }
        let initial_fill_targets = serde_json::to_string(&WsOutMsg::FillTargets(
            state_for_send.fill_targets_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_fill_targets).await.is_err() {
            return;
        }
        let model_clock_enabled = crate::telemetry_task::procedure_clock::now()
            .model_monotonic_ms()
            .is_some();
        let mut model_clock_samples =
            crate::telemetry_task::procedure_clock::HeldSamples::default();
        let initial_clock = state_for_send.launch_clock_presentation_snapshot();
        model_clock_samples.remember(&initial_clock);
        let initial_launch_clock =
            serde_json::to_string(&WsOutMsg::LaunchClock(initial_clock)).unwrap_or_default();
        if ws_out_tx.send(initial_launch_clock).await.is_err() {
            return;
        }
        let initial_board_status = serde_json::to_string(&WsOutMsg::BoardStatus(
            state_for_send.board_status_snapshot(crate::telemetry_task::get_current_timestamp_ms()),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_board_status).await.is_err() {
            return;
        }
        let initial_recording_status = serde_json::to_string(&WsOutMsg::RecordingStatus(
            state_for_send.recording_status_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_recording_status).await.is_err() {
            return;
        }
        let initial_board_abort = serde_json::to_string(&WsOutMsg::BoardAbort(
            crate::board_abort::snapshot_for_state(&state_for_send),
        ))
        .unwrap();
        if ws_out_tx.send(initial_board_abort).await.is_err() {
            return;
        }
        if ws_out_tx
            .send(
                serde_json::to_string(&WsOutMsg::AlertConditions(
                    state_for_send.alert_conditions.snapshot(),
                ))
                .unwrap(),
            )
            .await
            .is_err()
        {
            return;
        }
        let initial_alert_ack_state = serde_json::to_string(&WsOutMsg::AlertAckState(
            state_for_send.alert_ack_state_snapshot(),
        ))
        .unwrap_or_default();
        if ws_out_tx.send(initial_alert_ack_state).await.is_err() {
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
            timestamp_ms: crate::telemetry_task::displayed_network_utc_ms(),
        }))
        .unwrap_or_default();
        if ws_out_tx.send(initial_network_time).await.is_err() {
            return;
        }

        let adaptive_rate = std::env::var("GS_WS_ADAPTIVE_RATE").ok().as_deref() != Some("0");
        let mut last_chart_calibration = String::new();
        let mut network_time_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut model_clock_tick = tokio::time::interval(std::time::Duration::from_millis(250));
        model_clock_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        let ws_diagnostics = crate::ws_diagnostics_enabled();
        let mut last_ws_diag_log = Instant::now();
        let ws_diag_interval = StdDuration::from_secs(2);

        loop {
            tokio::select! {

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

                recv = board_abort_rx.recv() => {
                    if let Ok(snapshot)=recv
                        && let Ok(msg)=serde_json::to_string(&WsOutMsg::BoardAbort(snapshot))
                            && ws_out_tx.send(msg).await.is_err(){break;}
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

                recv = dashboard_reset_rx.recv() => {
                    match recv {
                        Ok(()) => {
                            let msg = WsOutMsg::DashboardReset;
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                changed = flight_maxima_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let maxima = flight_maxima_rx.borrow_and_update().clone();
                    let text = serde_json::to_string(&WsOutMsg::FlightMaxima(maxima)).unwrap_or_default();
                    if ws_out_tx.send(text).await.is_err() {
                        break;
                    }
                }
                changed = mission_phase_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let phase = mission_phase_rx.borrow_and_update().clone();
                    let text = serde_json::to_string(&WsOutMsg::MissionPhase(phase)).unwrap_or_default();
                    if ws_out_tx.send(text).await.is_err() {
                        break;
                    }
                }
                recv=alert_conditions_rx.recv()=>{
                    let snapshot=match recv {Ok(snapshot)=>snapshot,Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>state_for_send.alert_conditions.snapshot(),Err(tokio::sync::broadcast::error::RecvError::Closed)=>break};
                    if ws_out_tx.send(serde_json::to_string(&WsOutMsg::AlertConditions(snapshot)).unwrap()).await.is_err(){break;}
                }
                recv = alert_ack_rx.recv() => {
                    match recv {
                        Ok(ack) => {
                            let msg = WsOutMsg::AlertAckState(ack);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let msg = WsOutMsg::AlertAckState(state_for_send.alert_ack_state_snapshot());
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = state_rx.recv() => {
                    match recv {
                        Ok(fs) => {
                            let msg = WsOutMsg::FlightState(FlightStateWire::new(fs.state, &state_for_send));
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let msg = WsOutMsg::FlightState(FlightStateWire::new(
                                state_for_send.local_flight_state_snapshot(),
                                &state_for_send,
                            ));
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = launch_clock_rx.recv() => {
                    match recv {
                        Ok(clock) => {
                            let clock = if model_clock_enabled {
                                state_for_send.launch_clock_presentation_snapshot()
                            } else { clock };
                            model_clock_samples.remember(&clock);
                            let msg = WsOutMsg::LaunchClock(clock);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let clock=state_for_send.launch_clock_presentation_snapshot();
                            model_clock_samples.remember(&clock);
                            let msg = WsOutMsg::LaunchClock(clock);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let now_ms = crate::telemetry_task::get_current_timestamp_ms();
                            let msg = WsOutMsg::BoardStatus(state_for_send.board_status_snapshot(now_ms));
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                            let topology = WsOutMsg::NetworkTopology(
                                state_for_send.network_topology_snapshot(now_ms),
                            );
                            let text = serde_json::to_string(&topology).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = notifications_rx.recv() => {
                    match recv {
                        Ok(snapshot) => {
                            // A snapshot is history, never a popup. Announce only
                            // events observed live after this connection began.
                            for event in new_notification_events(&snapshot, &mut last_notification_id) {
                                let text = serde_json::to_string(&WsOutMsg::Notification(event)).unwrap_or_default();
                                if ws_out_tx.send(text).await.is_err() { return; }
                            }
                            let msg = WsOutMsg::Notifications(snapshot);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let snapshot = state_for_send.notifications_snapshot();
                            // Events missed by a slow client belong only in its
                            // Notification Center, not a burst of stale popups.
                            last_notification_id = snapshot.iter().map(|n| n.id).max().unwrap_or(last_notification_id);
                            let msg = WsOutMsg::Notifications(snapshot);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = messages_rx.recv() => {
                    match recv {
                        Ok(snapshot) => {
                            let msg = WsOutMsg::Messages(snapshot);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let msg = WsOutMsg::Messages(state_for_send.messages_snapshot());
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                changed = station_mode_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    station_mode_rx.borrow_and_update();
                    if let Some(change) = crate::station_mode_switch::last_change() {
                        let text = serde_json::to_string(&WsOutMsg::StationModeChanged(change)).unwrap_or_default();
                        if ws_out_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                    let msg = WsOutMsg::ActionPolicy(state_for_send.action_policy_snapshot());
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    if ws_out_tx.send(text).await.is_err() {
                        break;
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            crate::sequences::refresh_action_policy_now(&state_for_send);
                            let msg = WsOutMsg::ActionPolicy(state_for_send.action_policy_snapshot());
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let msg = WsOutMsg::FillTargets(state_for_send.fill_targets_snapshot());
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            let msg = WsOutMsg::RecordingStatus(
                                state_for_send.recording_status_snapshot(),
                            );
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if ws_out_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                _ = model_clock_tick.tick(), if model_clock_enabled => {
                    // UI-only250ms polling, deduplicated while paused. Never call
                    // set_launch_clock or the SEDSnet/DAQ publisher from here.
                    if let Some(clock)=model_clock_samples.changed(state_for_send.launch_clock_presentation_snapshot()) {
                        let text=serde_json::to_string(&WsOutMsg::LaunchClock(clock)).unwrap_or_default();
                        if ws_out_tx.send(text).await.is_err() { break; }
                    }
                }

                _ = network_time_tick.tick() => {
                    let calibration = serde_json::to_string(&*state_for_send.loadcell_calibration.lock().unwrap()).unwrap_or_default();
                    if calibration != last_chart_calibration {
                        last_chart_calibration = calibration;
                        telemetry_pending.clear();
                        let msg = serde_json::to_string(&WsOutMsg::CalibrationChanged).unwrap_or_default();
                        if ws_out_tx.send(msg).await.is_err() { break; }
                    }
                    let msg = WsOutMsg::NetworkTime(NetworkTimeMsg {
                        timestamp_ms: crate::telemetry_task::displayed_network_utc_ms(),
                    });
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    if ws_out_tx.send(text).await.is_err() {
                        break;
                    }
                    // Keep traffic rates current even when every router side is idle.
                    let topology = WsOutMsg::NetworkTopology(state_for_send.network_topology_snapshot(
                        crate::telemetry_task::get_current_timestamp_ms(),
                    ));
                    if ws_out_tx.send(serde_json::to_string(&topology).unwrap_or_default()).await.is_err() {
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
                            if ws_diagnostics && last_ws_diag_log.elapsed() >= ws_diag_interval {
                                eprintln!(
                                    "ws telemetry enqueue: pending={} dynamic_max_per_flush={} out_queue_cap={}",
                                    telemetry_pending.len(),
                                    dynamic_max_per_flush,
                                    ws_out_queue_cap,
                                );
                                last_ws_diag_log = Instant::now();
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if ws_diagnostics {
                                eprintln!(
                                    "ws telemetry broadcast lagged; pending_before_reseed={}",
                                    telemetry_pending.len()
                                );
                            }
                            let mut snapshot = state_for_send.recent_telemetry_snapshot();
                            let snapshot_cap =
                                telemetry_pending_cap.min(max_telemetry_per_flush_cap.saturating_mul(4));
                            if snapshot.len() > snapshot_cap {
                                let drop_n = snapshot.len() - snapshot_cap;
                                snapshot.drain(0..drop_n);
                            }
                            snapshot.sort_by_key(|row| row.timestamp_ms);
                            telemetry_pending = snapshot.into_iter().collect();
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
                    let row_count = rows.len();

                    if ws_diagnostics && last_ws_diag_log.elapsed() >= ws_diag_interval {
                        eprintln!(
                            "ws telemetry flush: rows={} pending_before={} dynamic_max_per_flush={}",
                            row_count,
                            pending_before,
                            dynamic_max_per_flush,
                        );
                        last_ws_diag_log = Instant::now();
                    }
                    let queue_was_full = live_telemetry.publish(rows);
                    if ws_diagnostics && queue_was_full {
                        eprintln!(
                            "ws telemetry coalesced unsent display batch: rows={}",
                            row_count
                        );
                    }

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

    // Session close is observed only between frames. An authenticated/manual
    // frame already running is allowed to finish and retains its own receipt.
    let mut registry_closing = state.control_ingress.subscribe();
    let (session_close, mut session_closing) = tokio::sync::watch::channel(false);
    let receive_registry = state.control_ingress.clone();
    let mut recv_task = tokio::spawn(async move {
        loop {
            if *registry_closing.borrow() || *session_closing.borrow() {
                break;
            }
            let message = tokio::select! {biased;
                _=registry_closing.changed()=>break,
                _=session_closing.changed()=>break,
                message=receiver.next()=>message,
            };
            let Some(Ok(message)) = message else {
                break;
            };
            if let Message::Text(text) = message {
                let Some(frame) = receive_registry.handler() else {
                    break;
                };
                frame
                    .run(handle_ws_control_text_with_source(
                        state_for_recv.clone(),
                        token.clone(),
                        cmd_tx.clone(),
                        text.to_string(),
                        {
                            let source = session_closing.clone();
                            Arc::new(move || !*source.borrow())
                        },
                    ))
                    .await;
            }
        }
    });
    let received = tokio::select! {
        _=send_task=>false,
        result=&mut recv_task=>{if result.is_err(){write_task.abort();let _=write_task.await;return;}true},
        _=&mut write_task=>false,
    };
    session_close.send_replace(true);
    let healthy = received || recv_task.await.is_ok();
    write_task.abort();
    // The writer emits no controls but must not outlive the terminal session.
    // Join only if it was not already consumed by select.
    if !write_task.is_finished() {
        let _ = write_task.await;
    }
    if healthy {
        connection.finish();
    }
}

pub(crate) async fn handle_ws_control_text_with_source(
    state_for_recv: Arc<AppState>,
    token: Option<String>,
    cmd_tx: mpsc::Sender<TelemetryCommand>,
    text: String,
    source_current: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    // Each frame retains its origin across authentication/database waits.
    let frame_name = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("cmd").and_then(|v| v.as_str()).map(str::to_owned));
    let phase_origin = serde_json::from_str::<WsCommand>(&text).ok()
        .map(|cmd|crate::operator_phase::seal(&state_for_recv,cmd.cmd));
    let safe_manual = phase_origin.as_ref().and_then(|cmd| crate::operator_queue::safe_manual::Intent::capture(&state_for_recv, cmd));
    let mut recovery_origin = phase_origin.as_ref().and_then(|cmd| crate::operator_queue::manual_origin::Origin::capture(&state_for_recv, cmd));
    let fill_origin = (frame_name.as_deref() == Some("StartFill"))
        .then(|| crate::fill_recording::Captured::capture(&state_for_recv));
    let admission_generation = if frame_name.as_deref() == Some("Abort") {
        0
    } else {
        crate::gse::capture_operator_origin(&state_for_recv).or_else(||recovery_origin.map(|origin|origin.generation())).unwrap_or(0)
    };
    let launch_revision = frame_name
        .as_deref()
        .and_then(|name| crate::board_abort::capture_launch_revision_named(&state_for_recv, name));
    let remote_launch_origin = frame_name
        .as_deref()
        .and_then(|name| crate::command_proxy::capture_rrls_launch_origin(&state_for_recv, name));
    let plumbing_source_ms =
        if crate::rrls::is_rrls() && frame_name.as_deref() == Some("RetractPlumbing") {
            state_for_recv.topology_router.get().and_then(|router| {
                crate::system_clock::required_control_network_utc_ms(router).ok()
            })
        } else {
            None
        };
    let plumbing_origin = serde_json::from_str::<WsCommand>(&text)
        .ok()
        .and_then(|command| {
            crate::actuator_plumbing::capture_motion_origin(&state_for_recv, &command.cmd)
        });
    #[cfg(test)]
    command_dispatch_tests::notify_ws_command_received(&state_for_recv);
    let principal = match state_for_recv
        .auth
        .authorize_token(
            &state_for_recv.auth_db,
            token.as_deref(),
            Permission::SendCommands,
        )
        .await
    {
        Ok(principal) => principal,
        Err(_) => {
            emit_notification_warning(
                &state_for_recv,
                "Command session expired or no longer has permission.",
            );
            return;
        }
    };
    if crate::rrls::is_rrls()
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(name) = value.get("cmd").and_then(|value| value.as_str())
        && !crate::command_proxy::rrls_runs_locally(name)
    {
        let response = crate::command_proxy::submit_with_origins(
            &state_for_recv,
            &principal,
            token.as_deref(),
            crate::command_proxy::Operation::Command(name.into()),
            if name == "RetractPlumbing" {
                plumbing_source_ms
            } else {
                None
            },
            remote_launch_origin,
        )
        .await;
        if !response.status().is_success() {
            emit_notification_warning(
                &state_for_recv,
                "Primary command request was not admitted; inspect RRLS control status.",
            );
        }
        return;
    }
    match serde_json::from_str::<WsCommand>(&text) {
        Ok(mut cmd) => {
            if let Some(original)=phase_origin {cmd.cmd=original;}
            if !principal.permissions.send_commands {
                emit_warning(
                    &state_for_recv,
                    format!(
                        "Rejected websocket command {:?}: authenticated session lacks send-command permission",
                        cmd.cmd
                    ),
                );
                return;
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
                return;
            }
            if let Some(intent) = safe_manual {
                if !source_current() { return; }
                let work = state_for_recv.clone();
                tokio::spawn(async move {
                    let (status, message) = crate::operator_queue::safe_manual::dispatch(work.clone(), &principal, intent, Some(source_current)).await;
                    if !status.is_success() { emit_notification_warning(&work, message); }
                });
                return;
            }
            if matches!(cmd.cmd, TelemetryCommand::Abort) && !crate::rrls::is_rrls() {
                state_for_recv.manual_abort_intent.request();
                return;
            }
            if !crate::operator_phase::is_local(&cmd.cmd) && crate::board_abort::launch_command(command_name(&cmd.cmd)) {
                if let Err(error) =
                    crate::board_abort::validate_launch_revision(&state_for_recv, launch_revision)
                {
                    emit_notification_warning(&state_for_recv, error);
                    return;
                }
                if let Some(message) = crate::board_abort::blocked_message(&state_for_recv) {
                    emit_notification_warning(&state_for_recv, message);
                    return;
                }
            }
            if !command_allowed_waiting_out_core(&state_for_recv,&cmd.cmd).await {
                emit_notification_warning(&state_for_recv,command_disabled_message(&state_for_recv,&cmd.cmd));
                return;
            }
            let launch_hold = match admit_launch_press(&state_for_recv, &cmd.cmd) {
                Ok(hold) => hold,
                Err(message) => {
                    emit_notification_warning(&state_for_recv, message);
                    return;
                }
            };
            let now_ms = software_command_clock_ms();
            let mut admission_rejection=None;
            match crate::gse::record_operator_command_with_recovery_origin_diagnostic(
                &state_for_recv,
                admission_generation,
                &cmd.cmd,
                now_ms,
                software_command_dedup_ms(),
                launch_revision,
                crate::gse::RecoveryAdmissionDiagnostic {
                    origin: &mut recovery_origin,
                    rejection: &mut admission_rejection,
                },
            ) {
                Err(message) => {
                    emit_notification_warning(&state_for_recv, message);
                    return;
                }
                Ok(crate::gse::OperatorCommandAdmission::Stale) => {
                    let message=admission_rejection.unwrap_or(crate::operator_queue::manual_origin::Rejection::MissingOrigin).message();
                    log::warn!("Command {} was not admitted: {}",cmd_name,message);
                    emit_notification_warning(&state_for_recv, message);
                    return;
                }
                Ok(crate::gse::OperatorCommandAdmission::Duplicate) => {
                    let fill_retry = matches!(cmd.cmd, TelemetryCommand::StartFill)
                        && state_for_recv.fill_recording.ready_instance() == 0;
                    if !fill_retry {
                        gs_debug_println!("Ignored duplicate websocket command {:?}", cmd.cmd);
                        return;
                    }
                    if state_for_recv
                        .fill_recording
                        .preparation
                        .available_permits()
                        == 0
                    {
                        crate::fill_recording::notify_failure(
                            &state_for_recv,
                            &crate::fill_recording::Error::Busy,
                        );
                        return;
                    }
                }
                Ok(crate::gse::OperatorCommandAdmission::Fresh) => {}
            }
            if matches!(cmd.cmd, TelemetryCommand::ResetTClock) {
                let reset_state = state_for_recv.clone();
                // Do not stall this WebSocket's Abort/command receiver on disk or radio.
                tokio::spawn(async move {
                    let result = if crate::rrls::is_rrls() {
                        crate::launch_clock_sync::request_reset(&reset_state).await
                    } else {
                        let work = reset_state.clone();
                        match tokio::task::spawn_blocking(move || {
                            reset_primary_clock_for_origin(&work, admission_generation)
                        })
                        .await
                        {
                            Ok(Ok(())) => Ok(true),
                            Ok(Err((_, error))) => Err(error),
                            Err(error) => Err(error.into()),
                        }
                    };
                    match result {
                        Ok(true) => {
                            reset_state.add_notification("Primary T clock reset saved");
                        }
                        Ok(false) => {
                            reset_state.add_notification(
                                "Primary T clock reset pending; retained clock unchanged",
                            );
                        }
                        Err(error) => {
                            reset_state
                                .add_notification(format!("T clock reset not confirmed: {error}"));
                        }
                    }
                });
                return;
            }
            if matches!(cmd.cmd, TelemetryCommand::StartFill) {
                let weak = Arc::downgrade(&state_for_recv);
                let authority = crate::fill_recording::Authority::Session {
                    token: token.clone(),
                    actor: principal.username.clone(),
                    admission_current: Arc::new(move || {
                        if !source_current() { return crate::gse::CurrentState::Invalid; }
                        weak.upgrade().map_or(crate::gse::CurrentState::Invalid, |state| fill_admission_state(&state))
                    }),
                };
                let Some(captured) = fill_origin else {
                    return;
                };
                let origin = captured.bind(authority, Some(&principal));
                crate::fill_recording::spawn(state_for_recv.clone(), origin);
                return;
            }
            let received_cmd = cmd.cmd;
            eprintln!("Accepted websocket command: {:?}", received_cmd);
            gs_debug_println!(
                "\x1b[32mWebsocket command received: {:?}\x1b[0m",
                received_cmd
            );
            if let Err(e) = cmd_tx
                .send(TelemetryCommand::QueuedOperator {
                    launch_hold,
                    recovery_origin,
                    launch_revision,
                    plumbing_origin,
                    generation: admission_generation,
                    command: Box::new(received_cmd),
                })
                .await
            {
                emit_notification_warning(
                    &state_for_recv,
                    "Command worker unavailable; command was not queued",
                );
                eprintln!("Failed to forward WS command to cmd_tx: {e}");
                gs_debug_println!("Failed to forward WS command to cmd_tx: {e}");
            }
        }
        Err(e) => {
            eprintln!("Invalid WS command JSON {text:?}: {e}");
            gs_debug_println!("Invalid WS command JSON {text:?}: {e}");
        }
    }
}

#[derive(Deserialize)]
struct HistoryParams {
    // /api/history?minutes=20  (defaults to 20 if not provided)
    minutes: Option<u64>,
}

/// NEW: /api/alerts – returns warnings + errors from `alerts` table
/// Query param: `minutes` (optional, defaults to 20)
async fn get_alert_conditions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    Json(state.alert_conditions.snapshot()).into_response()
}

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
    // Operator-cleared history stays hidden after reseeds and restarts.
    let cutoff = (now_ms - (minutes as i64) * 60_000)
        .max(state.alert_ack_state_snapshot().history_cleared_through_ms.saturating_add(1));

    let cols=sqlx::query("PRAGMA table_info(alerts)").fetch_all(&db).await.unwrap_or_default();
    let query=if cols.iter().any(|row|row.get::<String,_>("name")=="condition_json") {
        "SELECT timestamp_ms,severity,message,condition_json FROM alerts WHERE timestamp_ms BETWEEN ? AND ? ORDER BY timestamp_ms DESC"
    }else{
        "SELECT timestamp_ms,severity,message,NULL AS condition_json FROM alerts WHERE timestamp_ms BETWEEN ? AND ? ORDER BY timestamp_ms DESC"
    };
    let alerts_db = sqlx::query(query)
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
            condition:row.get::<Option<String>,_>("condition_json").and_then(|json|serde_json::from_str(&json).ok()),
        })
        .collect();

    for alert in cache_snapshot {
        if alert.timestamp_ms < cutoff || alert.timestamp_ms > now_ms {
            continue;
        }
        alerts.push(alert);
    }

    alerts.sort_by_key(|alert| std::cmp::Reverse(alert.timestamp_ms));
    alerts.dedup_by(|a, b| {
        a.timestamp_ms == b.timestamp_ms && a.severity == b.severity && a.message == b.message && a.condition==b.condition
    });

    Json(alerts).into_response()
}

async fn post_alert_ack(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AlertAckRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let ack = state.acknowledge_alerts_through(body.warning_timestamp_ms, body.error_timestamp_ms);
    crate::alert_history::save_acks(&state).await;
    Json(ack).into_response()
}

#[derive(Deserialize)]
struct AlertHistoryClearRequest {
    #[serde(default)]
    confirm: bool,
}

/// Clears resolved conditions and older warning/error events for every viewer.
/// Active conditions, board Abort latches and abort history are not touched.
async fn post_alert_history_clear(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AlertHistoryClearRequest>,
) -> axum::response::Response {
    let principal = match authorize_headers(&state, &headers, Permission::SendCommands).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let user = match principal.username {
        Some(name) if !principal.anonymous && !name.is_empty() => name,
        _ => {
            return (StatusCode::FORBIDDEN, "Sign in to clear alert history").into_response();
        }
    };
    if !body.confirm {
        return (
            StatusCode::BAD_REQUEST,
            "Confirm before clearing alert history",
        )
            .into_response();
    }
    match crate::alert_history::clear(&state, user).await {
        Ok(entry) => Json(entry).into_response(),
        Err((status, message)) => (status, message).into_response(),
    }
}

async fn get_alert_history_clears(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(response) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    match crate::alert_history::recent_entries(&state).await {
        Ok(entries) => Json(entries).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error).into_response(),
    }
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
    condition:Option<crate::alert_conditions::ConditionLink>,
) {
    state.cache_recent_alert(AlertDto {
        timestamp_ms,
        severity: severity.to_string(),
        message: message.clone(),
        condition:condition.clone(),
    });
    if let Err(err) = state
        .db_queue_tx
        .try_send(DbQueueItem::Write(DbWrite::Alert {
            timestamp_ms,
            severity: severity.to_string(),
            message,
            condition,
        }))
    {
        log::warn!("failed to queue {severity} alert DB write: {err}");
    }
}

/// PUBLIC HELPERS — can be called from *any thread* that has &AppState
///
/// Example usage from anywhere:
///     emit_warning(&app_state, "GPS fix lost");
///     emit_error(&app_state, "Main valve stuck closed");
///
/// If you only have Arc<AppState>, just pass `&*arc` or `arc.as_ref()`.
pub fn emit_warning<S: Into<String>>(state: &AppState, message: S) {
    emit_warning_linked(state, message, None);
}
pub fn emit_warning_linked<S: Into<String>>(
    state: &AppState,
    message: S,
    condition: Option<crate::alert_conditions::ConditionLink>,
) {
    let msg_string = message.into();
    let timestamp = now_ms_i64();

    // 1) Broadcast to frontend immediately
    let ws_msg = WarningMsg {
        timestamp_ms: timestamp,
        message: msg_string.clone(),
        condition:condition.clone(),
    };
    let _ = state.warnings_tx.send(ws_msg);

    // 2) Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "warning", msg_string, condition);
}

/// Log a warning to the DB without sending it to the frontend.
#[allow(dead_code)]
pub fn emit_warning_db_only<S: Into<String>>(state: &AppState, message: S) {
    let msg_string = message.into();
    let timestamp = now_ms_i64();

    // Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "warning", msg_string, None);
}

pub(crate) fn persist_message_db_only(state: &AppState, message: String) {
    let timestamp_ms = now_ms_i64();
    let id = state.next_message_id.fetch_add(1, Ordering::Relaxed) + 1;
    if let Err(err) = state
        .db_queue_tx
        .try_send(DbQueueItem::Write(DbWrite::Message {
            id,
            timestamp_ms,
            message,
            action_label: None,
            action_cmd: None,
        }))
    {
        log::warn!("failed to queue notification DB write: {err}");
    }
}

/// Sends a frontend notification while persisting the event in the DB without creating a warning alert.
pub fn emit_notification_warning<S: Into<String>>(state: &AppState, message: S) {
    let msg_string = message.into();
    state.add_temporary_notification(msg_string.clone());
    persist_message_db_only(state, msg_string);
}

pub fn emit_error<S: Into<String>>(state: &AppState, message: S) {
    emit_error_linked(state, message, None);
}
pub fn emit_error_linked<S: Into<String>>(
    state: &AppState,
    message: S,
    condition: Option<crate::alert_conditions::ConditionLink>,
) {
    emit_error_linked_at(state, now_ms_i64(), message, condition);
}
/// A deferred typed receipt keeps its original event time when restore finishes.
pub fn emit_error_linked_at<S: Into<String>>(
    state: &AppState,
    timestamp: i64,
    message: S,
    condition: Option<crate::alert_conditions::ConditionLink>,
) {
    let msg_string = message.into();

    // 1) Broadcast to frontend immediately
    let ws_msg = ErrorMsg {
        timestamp_ms: timestamp,
        message: msg_string.clone(),
        condition: condition.clone(),
    };
    let _ = state.errors_tx.send(ws_msg);

    // 2) Insert into DB asynchronously (tracked for graceful shutdown)
    spawn_alert_insert(state, timestamp, "error", msg_string, condition);
}

#[derive(Deserialize)]
struct ThermalZeroReq {
    sensor_id: String,
}

async fn thermal_settling_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(req): axum::extract::Query<ThermalZeroReq>,
) -> axum::response::Response {
    if let Err(e) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if !matches!(req.sensor_id.as_str(), "KG1000" | "KG50") {
        return (StatusCode::BAD_REQUEST, "Select KG1000 or KG50").into_response();
    }
    let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
    let rows = state.recent_telemetry_snapshot();
    let cfg = state.loadcell_calibration.lock().unwrap().clone();
    Json(crate::thermal_settling::from_rows(
        &rows,
        &cfg,
        &req.sensor_id,
        now,
    ))
    .into_response()
}

async fn capture_thermal_zero(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ThermalZeroReq>,
) -> axum::response::Response {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let principal = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    if !principal_can_edit_calibration(&principal) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if !matches!(req.sensor_id.as_str(), "KG1000" | "KG50") {
        return (StatusCode::BAD_REQUEST, "Select KG1000 or KG50").into_response();
    }
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let rows = state.recent_telemetry_snapshot();
    let calibration = state.loadcell_calibration.lock().unwrap().clone();
    let settling = crate::thermal_settling::from_rows(&rows, &calibration, &req.sensor_id, now);
    if !settling.ready {
        return (StatusCode::CONFLICT, settling.message).into_response();
    }
    let raw = settling.raw.unwrap() as f32;
    let temperature = settling.temperature_c.unwrap() as f32;
    let updated = {
        let mut slot = state.loadcell_calibration.lock().unwrap();
        let check = crate::thermal_settling::from_rows(&rows, &slot, &req.sensor_id, now);
        if !check.ready {
            return (StatusCode::CONFLICT, check.message).into_response();
        }
        let mut cfg = slot.clone();
        if let Err(err) = loadcell::capture_thermal_zero(&mut cfg, &req.sensor_id, raw, temperature)
        {
            return (StatusCode::BAD_REQUEST, err).into_response();
        }
        if let Err(err) = loadcell::save(&cfg) {
            return (StatusCode::INTERNAL_SERVER_ERROR, err).into_response();
        }
        *slot = cfg.clone();
        cfg
    };
    if let Err(err) = publish_daq_calibration(&state, &updated) {
        return (StatusCode::SERVICE_UNAVAILABLE, err).into_response();
    }
    Json(updated).into_response()
}

#[derive(Deserialize)]
struct LongZeroStart {
    sensor_id: String,
    duration_s: u64,
    known_zero: bool,
}
#[derive(Deserialize)]
struct LongZeroId {
    session_id: String,
}
#[derive(Deserialize)]
struct LongZeroApply {
    session_id: String,
    apply_thermal: bool,
    tau_ms: f32,
}
async fn long_zero_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(e) = authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    Json(state.loadcell_processing.status()).into_response()
}
async fn long_zero_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LongZeroStart>,
) -> axum::response::Response {
    let p = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !principal_can_edit_calibration(&p) {
        return calibration_edit_forbidden_response();
    }
    match state.loadcell_processing.start_calibrated(
        &req.sensor_id,
        req.duration_s,
        req.known_zero,
        state.loadcell_calibration.lock().unwrap().clone(),
    ) {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}
async fn long_zero_stop(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LongZeroId>,
) -> axum::response::Response {
    let p = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !principal_can_edit_calibration(&p) {
        return calibration_edit_forbidden_response();
    }
    match state.loadcell_processing.stop(&req.session_id) {
        Ok(()) => Json(state.loadcell_processing.status()).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}
async fn long_zero_apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<LongZeroApply>,
) -> axum::response::Response {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let p = match authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !principal_can_edit_calibration(&p) {
        return calibration_edit_forbidden_response();
    }
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    let updated = {
        let mut slot = state.loadcell_calibration.lock().unwrap();
        let mut cfg = slot.clone();
        if let Err(e) = state.loadcell_processing.apply(
            &req.session_id,
            &mut cfg,
            req.apply_thermal,
            req.tau_ms,
        ) {
            return (StatusCode::BAD_REQUEST, e).into_response();
        }
        if let Err(e) = loadcell::save(&cfg) {
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
        *slot = cfg.clone();
        cfg
    };
    if let Err(e) = publish_daq_calibration(&state, &updated) {
        return (StatusCode::SERVICE_UNAVAILABLE, e).into_response();
    }
    state.broadcast_fill_targets_snapshot();
    Json(updated).into_response()
}

#[cfg(test)]
mod recovery_direct_ingress_tests {
    use super::*;
    use futures::FutureExt;
    use std::sync::atomic::Ordering;
    pub(super) async fn fixture() -> (
        crate::recording_export::tests::TestDirectory,
        Arc<AppState>,
        HeaderMap,
        Arc<sedsnet::router::Router>,
        Arc<std::sync::Mutex<Vec<sedsnet::packet::Packet>>>,
    ) {
        let (root, state, _commands, headers) = command_dispatch_tests::preauth_fixture().await;
        let auth_path = root.0.join("auth.json");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
        auth["users"][0]["calibration_access"] = serde_json::json!({"view":true,"edit":true});
        std::fs::write(auth_path, auth.to_string()).unwrap();
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = observed.clone();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                    move |packet| {
                        sink.lock().unwrap().push(packet.clone());
                        Ok(())
                    },
                ),
            ])
            .with_sender("GS"),
        ));
        state.topology_router.set(router.clone()).unwrap();
        (root, state, headers, router, observed)
    }
    fn cross_epoch(state: &AppState) {
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        state.automatic_generation.fetch_add(1, Ordering::AcqRel);
        state.operator_admission_barrier.store(0, Ordering::Release);
    }
    async fn calibration_request(
        state: Arc<AppState>,
        headers: HeaderMap,
        kind: usize,
    ) -> axum::response::Response {
        match kind {
            0 => {
                let mut calibration = loadcell::LoadcellCalibrationFile::default();
                calibration.ch1.m = Some(2.0);
                set_loadcell_calibration(
                    State(state),
                    headers,
                    Json(loadcell::CalibrationUpdate {
                        channel: "ch1".into(),
                        calibration,
                    }),
                )
                .await
                .into_response()
            }
            1 => set_kg50_input(
                State(state),
                headers,
                Json(Kg50InputRequest {
                    input: loadcell::Kg50Input::P6Differential,
                }),
            )
            .await
            .into_response(),
            2 => capture_loadcell_zero(
                State(state),
                headers,
                Json(CaptureLoadcellPointReq {
                    sensor_id: "KG1000".into(),
                    raw: 0.123,
                }),
            )
            .await
            .into_response(),
            3 => capture_loadcell_span(
                State(state),
                headers,
                Json(CaptureLoadcellSpanReq {
                    sensor_id: "KG1000".into(),
                    raw: 1.234,
                    known_kg: 1.0,
                }),
            )
            .await
            .into_response(),
            4 => capture_thermal_zero(
                State(state),
                headers,
                Json(ThermalZeroReq {
                    sensor_id: "KG1000".into(),
                }),
            )
            .await
            .into_response(),
            _ => {
                let id = state.loadcell_processing.status().session_id;
                long_zero_apply(
                    State(state),
                    headers,
                    Json(LongZeroApply {
                        session_id: id,
                        apply_thermal: false,
                        tau_ms: 100.0,
                    }),
                )
                .await
                .into_response()
            }
        }
    }
    #[tokio::test]
    async fn kg50_input_selection_persists_and_is_compared_with_the_board_report() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "kg50_input_selection_persists_and_is_compared_with_the_board_report",
        ) {
            return;
        }
        use crate::kg50_input_sync::SyncState;
        use loadcell::Kg50Input;
        let (_root, state, headers, router, _) = fixture().await;
        let ty = crate::telemetry_schema::data_type("DAQ_KG50_INPUT");
        router
            .enable_network_variable(ty, sedsnet::router::NetworkVariablePermissions::READ_WRITE)
            .unwrap();
        let status = |state: &Arc<AppState>| {
            let selected = state.loadcell_calibration.lock().unwrap().kg50_input;
            state.kg50_input_sync.status(selected)
        };
        assert_eq!(status(&state).state, SyncState::NoReadback);
        state.kg50_input_sync.observe(Kg50Input::Amp2);
        assert_eq!(status(&state).state, SyncState::Synced);
        let previous_stamp = {
            let mut cfg = state.loadcell_calibration.lock().unwrap();
            // A revision ahead of the wall clock must still move forwards.
            cfg.kg50_input_stamp_ms = u64::MAX / 2;
            cfg.kg50_input_stamp_ms
        };
        let response = set_kg50_input(
            State(state.clone()),
            headers,
            Json(Kg50InputRequest {
                input: Kg50Input::P6Differential,
            }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let saved = state.loadcell_calibration.lock().unwrap().clone();
        assert_eq!(saved.kg50_input, Kg50Input::P6Differential);
        assert_eq!(saved.kg50_input_stamp_ms, previous_stamp + 1);
        assert_eq!(
            loadcell::load_or_default().kg50_input,
            Kg50Input::P6Differential,
            "a restarted station reads the same selection"
        );
        let sent = router.get_cached_network_variable(ty).unwrap().unwrap();
        assert_eq!(sent.payload()[..], [1]);
        assert_eq!(sent.timestamp(), saved.kg50_input_stamp_ms);
        // The board has not switched yet, so the UI must not claim it has.
        let reply = status(&state);
        assert_eq!(reply.state, SyncState::Syncing);
        assert_eq!(reply.board_input, Some(Kg50Input::Amp2));
        state.kg50_input_sync.observe(Kg50Input::P6Differential);
        assert_eq!(status(&state).state, SyncState::Synced);
        let wire = serde_json::to_value(status(&state)).unwrap();
        assert_eq!(wire["selected"], "p6_differential");
        assert_eq!(wire["board_input"], "p6_differential");
        assert_eq!(wire["state"], "synced");
    }
    #[tokio::test]
    async fn real_calibration_ingress_keeps_preauth_origin_before_file_ram_and_sdk_effects() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_calibration_ingress_keeps_preauth_origin_before_file_ram_and_sdk_effects",
        ) {
            return;
        }
        let mut observations = Vec::new();
        for kind in 0..6 {
            let (_root, state, headers, router, _) = fixture().await;
            if kind >= 4 {
                let mut cfg = state.loadcell_calibration.lock().unwrap();
                cfg.ch1.m = Some(1.0);
                cfg.ch1.b = Some(0.0);
            }
            if kind == 4 {
                let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
                let start = (now / 10000 - 12) * 10000;
                for at in (start..=now).step_by(1000) {
                    for (data_type, value) in [("DAQ_ADC_TEMPERATURE", 20.0), ("KG1000", 0.123)] {
                        state.cache_recent_telemetry(TelemetryRow {
                            timestamp_ms: at,
                            data_type: data_type.into(),
                            sender_id: "DAQ".into(),
                            values: vec![Some(value)],
                        });
                    }
                }
                assert!(
                    crate::thermal_settling::from_rows(
                        &state.recent_telemetry_snapshot(),
                        &state.loadcell_calibration.lock().unwrap(),
                        "KG1000",
                        now
                    )
                    .ready
                );
            }
            if kind == 5 {
                let cfg = state.loadcell_calibration.lock().unwrap().clone();
                let capture = state
                    .loadcell_processing
                    .start_calibrated("KG1000", 60, true, cfg)
                    .unwrap();
                for i in 0..1200 {
                    state.loadcell_processing.observe(
                        "DAQ",
                        "KG1000",
                        1000 + i * 100,
                        0.123,
                        Some(20.0),
                    );
                }
                tokio::time::timeout(std::time::Duration::from_secs(4), async {
                    while state.loadcell_processing.status().report.samples < 1200 {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                state.loadcell_processing.stop(&capture.session_id).unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while state.loadcell_processing.status().running {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(state.loadcell_processing.status().report.noise_ready);
            }
            let cfg_before =
                serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap();
            let path = loadcell::calibration_file_for_test();
            let file_before = std::fs::read(&path).ok();
            let held = state.auth_db.acquire().await.unwrap();
            assert_eq!((state.auth_db.size(), state.auth_db.num_idle()), (1, 0));
            let request = calibration_request(state.clone(), headers.clone(), kind);
            tokio::pin!(request);
            assert!(
                request.as_mut().now_or_never().is_none(),
                "actual first auth query must wait on held SQLite connection"
            );
            cross_epoch(&state);
            drop(held);
            let stale = request.await.status();
            let cfg_after_stale =
                serde_json::to_value(&*state.loadcell_calibration.lock().unwrap()).unwrap();
            let file_after_stale = std::fs::read(&path).ok();
            let cache_after_stale = router
                .get_cached_network_variable(crate::telemetry_schema::data_type(
                    "DAQ_LOADCELL_CALIBRATION",
                ))
                .unwrap()
                .is_some();
            let fresh = calibration_request(state.clone(), headers.clone(), kind)
                .await
                .status();
            let cache_after_fresh = router
                .get_cached_network_variable(crate::telemetry_schema::data_type(
                    "DAQ_LOADCELL_CALIBRATION",
                ))
                .unwrap()
                .is_some();
            if let Some(bytes) = &file_before {
                std::fs::write(&path, bytes).unwrap();
            } else {
                let _ = std::fs::remove_file(&path);
            }
            observations.push((
                kind,
                fresh,
                stale,
                cache_after_stale,
                cfg_after_stale == cfg_before,
                file_after_stale == file_before,
                cache_after_fresh,
            ));
        }
        eprintln!("actual calibration HTTP observations: {observations:?}");
        for (kind, fresh, stale, old_cache, same_ram, same_file, fresh_cache) in observations {
            assert_eq!(
                fresh,
                StatusCode::OK,
                "current authorized calibration kind={kind}"
            );
            assert!(
                !old_cache,
                "old auth-delayed calibration must not reach actual SDK kind={kind}"
            );
            assert!(
                same_ram && same_file,
                "stale request must not change disk/RAM kind={kind}"
            );
            assert_eq!(stale, StatusCode::CONFLICT);
            assert!(fresh_cache);
        }
    }
    #[tokio::test]
    async fn real_flight_setup_preserves_existing_schema_refusal_and_origin_fence() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_flight_setup_preserves_existing_schema_refusal_and_origin_fence",
        ) {
            return;
        }
        let (_root, state, headers, router, observed) = fixture().await;
        let mut warnings = state.warnings_tx.subscribe();
        let held = state.auth_db.acquire().await.unwrap();
        let request = apply_flight_setup(State(state.clone()), headers.clone());
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        cross_epoch(&state);
        drop(held);
        let stale = request.await.into_response().status();
        router.process_all_queues().unwrap();
        let after_stale = observed.lock().unwrap().len();
        let fresh = apply_flight_setup(State(state.clone()), headers)
            .await
            .into_response()
            .status();
        router.process_all_queues().unwrap();
        let mut diagnostics = Vec::new();
        while let Ok(message) = warnings.try_recv() {
            diagnostics.push(message.message);
        }
        eprintln!("actual flight-setup SDK refusal: {diagnostics:?}");
        assert_eq!(
            fresh,
            StatusCode::BAD_GATEWAY,
            "existing static one-byte schema refusal must remain honest"
        );
        assert_eq!(
            after_stale, 0,
            "old handler actually reached the flight SDK owner"
        );
        assert_eq!(stale, StatusCode::CONFLICT);
        assert!(observed.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn real_sd_retry_ingress_cannot_consume_admission_before_fresh_request() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_sd_retry_ingress_cannot_consume_admission_before_fresh_request",
        ) {
            return;
        }
        let (_root, state, headers, router, _) = fixture().await;
        let now = crate::telemetry_task::get_current_timestamp_ms();
        state.network_health.lock().unwrap().observe_sd_status(
            "DAQ",
            &[1, u32::MAX, u32::MAX, 1, 0, 0, 0, 0],
            std::time::Instant::now(),
            now,
        );
        let held = state.auth_db.acquire().await.unwrap();
        let request = crate::daq_sd::retry(State(state.clone()), headers.clone());
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        cross_epoch(&state);
        drop(held);
        let stale = request.await.status();
        let queued_after_stale = router.export_runtime_stats().queues.tx_len;
        let fresh = crate::daq_sd::retry(State(state.clone()), headers)
            .await
            .status();
        assert_eq!(
            queued_after_stale, 0,
            "old auth-delayed retry actually enqueued SDK work"
        );
        assert_eq!(
            fresh,
            StatusCode::ACCEPTED,
            "fresh request must succeed without clearing the existing retry gate"
        );
        assert_eq!(stale, StatusCode::CONFLICT);
    }
    #[tokio::test]
    async fn direct_ingress_keeps_current_auth_permissions_ahead_of_closed_admission() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "direct_ingress_keeps_current_auth_permissions_ahead_of_closed_admission",
        ) {
            return;
        }
        let (root, state, headers, router, _) = fixture().await;
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        let unauth = apply_flight_setup(State(state.clone()), HeaderMap::new())
            .await
            .into_response()
            .status();
        assert!(matches!(
            unauth,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ));
        let path = root.0.join("auth.json");
        let mut auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        auth["users"][0]["permissions"]["send_commands"] = serde_json::json!(false);
        auth["users"][0]["calibration_access"]["edit"] = serde_json::json!(false);
        std::fs::write(path, auth.to_string()).unwrap();
        assert_eq!(
            apply_flight_setup(State(state.clone()), headers.clone())
                .await
                .into_response()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            crate::daq_sd::retry(State(state.clone()), headers.clone())
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            calibration_request(state.clone(), headers, 0)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(router.export_runtime_stats().queues.tx_len, 0);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
}

#[cfg(test)]
mod recovery_control_settings_tests {
    use super::*;
    use futures::FutureExt;
    use std::sync::atomic::Ordering;
    #[derive(Clone)]
    enum Desired {
        Automatic(crate::auto_zero::Settings),
        Fill(FillTargetsConfig),
        Flight(FlightSetupConfig),
    }
    async fn request(
        state: Arc<AppState>,
        headers: HeaderMap,
        desired: Desired,
    ) -> axum::response::Response {
        match desired {
            Desired::Automatic(value) => set_auto_zero(State(state), headers, Json(value.into()))
                .await
                .into_response(),
            Desired::Fill(value) => set_fill_targets(State(state), headers, Json(value))
                .await
                .into_response(),
            Desired::Flight(value) => set_flight_setup(State(state), headers, Json(value))
                .await
                .into_response(),
        }
    }
    fn snapshot(state: &AppState, kind: usize) -> serde_json::Value {
        match kind {
            0 => serde_json::to_value(state.auto_zero.lock().unwrap().settings()).unwrap(),
            1 => serde_json::to_value(&*state.fill_targets.lock().unwrap()).unwrap(),
            _ => serde_json::to_value(flight_setup::load_or_default()).unwrap(),
        }
    }
    #[tokio::test]
    async fn real_control_settings_keep_preauth_origin_across_new_run() {
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_control_settings_keep_preauth_origin_across_new_run",
        ) {
            return;
        }
        let mut observations = Vec::new();
        for kind in 0..3 {
            let (_root, state, headers, _network, _) =
                recovery_direct_ingress_tests::fixture().await;
            let desired = match kind {
                0 => Desired::Automatic(crate::auto_zero::Settings {
                    enabled: !state.auto_zero.lock().unwrap().settings().enabled,
                    before_launch: false,
                }),
                1 => {
                    let mut value = state.fill_targets.lock().unwrap().clone();
                    value.nitrous.target_mass_kg += 1.0;
                    Desired::Fill(value)
                }
                _ => {
                    let mut value = flight_setup::load_or_default();
                    value.selected_profile_id = value
                        .profiles
                        .iter()
                        .find(|p| p.id != value.selected_profile_id)
                        .unwrap()
                        .id
                        .clone();
                    Desired::Flight(value)
                }
            };
            let path = match kind {
                0 => std::path::PathBuf::from(std::env::var_os("GS_AUTO_ZERO_CONFIG").unwrap()),
                1 => fill_targets::config_path(),
                _ => flight_setup::config_path(),
            };
            let before = snapshot(&state, kind);
            let file_before = std::fs::read(&path).ok();
            let held = state.auth_db.acquire().await.unwrap();
            let future = request(state.clone(), headers.clone(), desired.clone());
            tokio::pin!(future);
            assert!(future.as_mut().now_or_never().is_none());
            state
                .operator_admission_barrier
                .store(77, Ordering::Release);
            state.automatic_generation.fetch_add(1, Ordering::AcqRel);
            state.operator_admission_barrier.store(0, Ordering::Release);
            drop(held);
            let stale = future.await.status();
            let same_ram = snapshot(&state, kind) == before;
            let same_file = std::fs::read(&path).ok() == file_before;
            let fresh = request(state.clone(), headers, desired).await.status();
            let changed = snapshot(&state, kind) != before;
            if let Some(bytes) = file_before {
                std::fs::write(&path, bytes).unwrap();
            } else {
                let _ = std::fs::remove_file(&path);
            }
            observations.push((kind, stale, fresh, same_ram, same_file, changed));
        }
        eprintln!("actual control-settings observations: {observations:?}");
        for (kind, stale, fresh, same_ram, same_file, changed) in observations {
            assert!(
                same_ram && same_file,
                "old settings request changed new run kind={kind}"
            );
            assert_eq!(stale, StatusCode::CONFLICT);
            assert_eq!(fresh, StatusCode::OK);
            assert!(changed);
        }
    }
}

#[cfg(test)]
mod alert_condition_readonly_tests {
    include!("alert_condition_readonly_tests.rs");
}

#[cfg(test)]
#[path = "operator_queue/safe_manual_http_tests.rs"]
mod safe_manual_http_tests;

#[cfg(test)]
mod alert_history_clear_tests {
    use super::*;
    use crate::alert_conditions::{BoardKey, ConditionState, Key};

    fn error_event(timestamp_ms: i64) -> AlertDto {
        AlertDto {
            timestamp_ms,
            severity: "error".into(),
            message: format!("fixture error {timestamp_ms}"),
            condition: None,
        }
    }

    async fn alert_times(state: &Arc<AppState>, headers: &HeaderMap) -> Vec<i64> {
        let response = get_alerts(
            State(state.clone()),
            headers.clone(),
            Query(HistoryParams { minutes: Some(60) }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice::<Vec<serde_json::Value>>(&body)
            .unwrap()
            .iter()
            .map(|alert| alert["timestamp_ms"].as_i64().unwrap())
            .collect()
    }

    async fn acknowledge(state: &Arc<AppState>, headers: &HeaderMap, warning: i64, error: i64) {
        let response = post_alert_ack(
            State(state.clone()),
            headers.clone(),
            Json(AlertAckRequest { warning_timestamp_ms: warning, error_timestamp_ms: error }),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// What a station restart does to the shared acknowledgement state.
    async fn restart(state: &Arc<AppState>) -> (i64, i64) {
        *state.alert_ack_state.lock().unwrap() = AlertAckStateMsg::default();
        crate::alert_history::restore(state).await;
        let ack = state.alert_ack_state_snapshot();
        (ack.warning_ack_timestamp_ms, ack.error_ack_timestamp_ms)
    }

    #[tokio::test]
    async fn acknowledgements_survive_a_restart_without_hiding_a_new_occurrence() {
        let (_root, state, _commands, headers) =
            command_dispatch_tests::preauth_fixture().await;
        let now = now_ms_i64();
        acknowledge(&state, &headers, now - 2_000, now - 1_000).await;
        assert_eq!(restart(&state).await, (now - 2_000, now - 1_000), "acknowledgements survive a restart");
        assert_eq!(restart(&state).await, (now - 2_000, now - 1_000), "and a second one");

        // The same alert raised again after the restart is a new occurrence,
        // stamped after the saved acknowledgement, so it shows unacknowledged.
        state.cache_recent_alert(error_event(now_ms_i64()));
        let newest = state.recent_alerts_snapshot().iter().map(|a| a.timestamp_ms).max().unwrap();
        assert!(newest > state.alert_ack_state_snapshot().error_ack_timestamp_ms);

        // A saved acknowledgement ahead of the clock the station restarts with
        // (no RTC, or a client clock ahead) never covers alerts raised after
        // the restart: it is cut back to the restart time.
        acknowledge(&state, &headers, now + 3_600_000, now + 3_600_000).await;
        let (warning, error) = restart(&state).await;
        let after = now_ms_i64();
        assert!(warning <= after && error <= after, "{warning} {error} > {after}");
        assert!(error >= now - 1_000, "earlier acknowledgements are kept");
    }

    #[tokio::test]
    async fn clearing_history_keeps_active_conditions_requires_a_signed_in_confirmation_and_is_audited() {
        let (_root, state, _commands, headers) =
            command_dispatch_tests::preauth_fixture().await;
        crate::alert_condition_runtime::restore_empty_for_test(&state);
        let fc_abort = Key::BoardAbort(BoardKey::Fc);
        let ab_abort = Key::BoardAbort(BoardKey::Ab);
        for (state_now, at) in [(ConditionState::Active, 10), (ConditionState::Resolved, 11)] {
            crate::alert_condition_runtime::observe(&state, fc_abort, state_now, "FC Abort".into(), at);
        }
        crate::alert_condition_runtime::observe(&state, ab_abort, ConditionState::Active, "AB Abort".into(), 12);
        let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
        state.cache_recent_alert(error_event(now - 2_000));
        state.cache_recent_alert(error_event(now - 1_000));
        assert_eq!(alert_times(&state, &headers).await.len(), 2);

        let anonymous = post_alert_history_clear(
            State(state.clone()),
            HeaderMap::new(),
            Json(AlertHistoryClearRequest { confirm: true }),
        )
        .await;
        assert!(anonymous.status().is_client_error());
        let unconfirmed = post_alert_history_clear(
            State(state.clone()),
            headers.clone(),
            Json(AlertHistoryClearRequest { confirm: false }),
        )
        .await;
        assert_eq!(unconfirmed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(alert_times(&state, &headers).await.len(), 2, "nothing cleared without confirmation");

        let mut ack_rx = state.alert_ack_tx.subscribe();
        let cleared = post_alert_history_clear(
            State(state.clone()),
            headers.clone(),
            Json(AlertHistoryClearRequest { confirm: true }),
        )
        .await;
        assert_eq!(cleared.status(), StatusCode::OK);
        let body = axum::body::to_bytes(cleared.into_body(), 4096).await.unwrap();
        let entry: crate::alert_history::ClearEntry = serde_json::from_slice(&body).unwrap();
        assert_eq!(entry.cleared_by, "operator");
        assert_eq!(entry.resolved_conditions_cleared, 1);
        assert!(entry.cleared_through_ms >= now - 1_000);
        assert_eq!(
            ack_rx.try_recv().unwrap().history_cleared_through_ms,
            entry.cleared_through_ms,
            "every viewer is told to drop the cleared events"
        );

        assert!(alert_times(&state, &headers).await.is_empty());
        assert!(state.recent_alerts_snapshot().is_empty(), "alarm cache no longer holds cleared events");
        let records = state.alert_conditions.snapshot().records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, ab_abort.wire());
        assert_eq!(records[0].state, ConditionState::Active, "active errors are never cleared");
        assert_eq!(state.alert_conditions.current_state(ab_abort), Some(ConditionState::Active));

        state.cache_recent_alert(error_event(entry.cleared_through_ms + 1));
        assert_eq!(alert_times(&state, &headers).await, vec![entry.cleared_through_ms + 1]);

        let audit = get_alert_history_clears(State(state.clone()), headers.clone()).await;
        let body = axum::body::to_bytes(audit.into_body(), 4096).await.unwrap();
        let entries: Vec<crate::alert_history::ClearEntry> = serde_json::from_slice(&body).unwrap();
        assert_eq!(entries, vec![entry.clone()]);

        state.clear_alert_history_through(0);
        *state.alert_ack_state.lock().unwrap() = AlertAckStateMsg::default();
        crate::alert_history::restore(&state).await;
        assert_eq!(
            state.alert_ack_state_snapshot().history_cleared_through_ms,
            entry.cleared_through_ms,
            "a restart keeps cleared events hidden"
        );
    }
}
