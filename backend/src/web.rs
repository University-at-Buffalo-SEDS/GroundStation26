use crate::layout;
use crate::map::{tile_service, DEFAULT_MAP_REGION};
use crate::state::AppState;
use axum::http::{header, StatusCode};
use axum::{
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade}, extract::{Query, State},
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use futures::{SinkExt, StreamExt};
use groundstation_shared::{BoardStatusMsg, FlightState, TelemetryCommand, TelemetryRow};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::OnceCell;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;

// NEW

static FAVICON_DATA: OnceCell<Vec<u8>> = OnceCell::const_new();

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
    let tiles_dir = tile_service(DEFAULT_MAP_REGION); // NEW

    Router::new()
        .layer(CompressionLayer::new())
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/alerts", get(get_alerts))
        .route("/api/boards", get(get_boards))
        .route("/api/layout", get(get_layout))
        .route("/favicon", get(get_favicon))
        .route("/flightstate", get(get_flight_state))
        .route("/api/gps", get(get_gps))
        .route("/ws", get(ws_handler))
        .nest_service("/tiles", tiles_dir)
        .route("/favicon.ico", get(get_favicon))
        .route("/valvestate", get(get_valve_state))
        // anything that doesn’t match the above routes goes to the static files
        .fallback_service(static_dir)
        .with_state(state)
}

/// Outgoing WebSocket messages to the frontend.
/// This is what the frontend will deserialize:
///   { "ty": "telemetry", "data": { ...TelemetryRow... } }
///   { "ty": "warning",   "data": { ...WarningMsg... } }
///   { "ty": "error",     "data": { ...ErrorMsg... } }
#[derive(Serialize)]
#[serde(tag = "ty", content = "data")]
pub enum WsOutMsg {
    Telemetry(TelemetryRow),
    Warning(WarningMsg),
    FlightState(FlightStateMsg),
    Error(ErrorMsg),
    BoardStatus(BoardStatusMsg),
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
        Ok(layout) => Json(layout).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err).into_response(),
    }
}

async fn get_recent(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let latest_ts: Option<i64> = sqlx::query_scalar("SELECT MAX(timestamp_ms) FROM telemetry")
        .fetch_one(&state.db)
        .await
        .ok()
        .flatten();

    let Some(now_ms) = latest_ts else {
        return Json(Vec::<TelemetryRow>::new());
    };

    let cutoff = now_ms - 20 * 60 * 1000; // 20 minutes

    // Match frontend bucket grid (frontend BUCKET_MS = 20).
    // This returns at most 1 row per (data_type, 20ms bucket) in the window:
    // - Uses REAL rows (no averaging).
    // - Picks the latest timestamp within each bucket for each data_type.
    // - Keeps chronological order for the frontend.
    const BUCKET_MS: i64 = 20;

    let rows_db = sqlx::query(
        r#"
        WITH filtered AS (
            SELECT
                timestamp_ms,
                data_type,
                values_json,
                payload_json,
                (timestamp_ms / ?) AS bucket_id
            FROM telemetry
            WHERE timestamp_ms BETWEEN ? AND ?
        ),
        latest AS (
            SELECT data_type, bucket_id, MAX(timestamp_ms) AS ts
            FROM filtered
            GROUP BY data_type, bucket_id
        )
        SELECT
            f.timestamp_ms, f.data_type,
            f.values_json, f.payload_json
        FROM filtered f
        JOIN latest l
          ON l.data_type = f.data_type
         AND l.bucket_id = f.bucket_id
         AND l.ts = f.timestamp_ms
        ORDER BY f.timestamp_ms ASC
        "#,
    )
        .bind(BUCKET_MS)
        .bind(cutoff)
        .bind(now_ms)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

    let rows: Vec<TelemetryRow> = rows_db
        .into_iter()
        .map(|row| TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            values: values_from_row(&row),
        })
        .collect();

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

    let cmd_tx = state.cmd_tx.clone();
    let (mut sender, mut receiver) = socket.split();

    // Task: server -> client (all streams multiplexed)
    let send_task = async move {
        const BUCKET_MS: i64 = 20;
        const TELEMETRY_FLUSH_MS: u64 = 50;
        const MAX_TELEMETRY_PER_FLUSH: usize = 24;
        let mut telemetry_latest_by_type: HashMap<String, TelemetryRow> = HashMap::new();
        let mut telemetry_flush =
            tokio::time::interval(std::time::Duration::from_millis(TELEMETRY_FLUSH_MS));

        loop {
            tokio::select! {
                biased;

                recv = warnings_rx.recv() => {
                    match recv {
                        Ok(warn) => {
                            let msg = WsOutMsg::Warning(warn);
                            let text = serde_json::to_string(&msg).unwrap_or_default();
                            if sender
                                .send(Message::Text(Utf8Bytes::from(text)))
                                .await
                                .is_err()
                            {
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
                            if sender.send(Message::Text(Utf8Bytes::from(text))).await.is_err() {
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
                            if sender.send(Message::Text(Utf8Bytes::from(text))).await.is_err() {
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
                            if sender
                                .send(Message::Text(Utf8Bytes::from(text)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                recv = telemetry_rx.recv() => {
                    match recv {
                        Ok(pkt) => {
                            let bucket_id = pkt.timestamp_ms / BUCKET_MS;
                            let key = pkt.data_type.clone();
                            let replace = telemetry_latest_by_type
                                .get(&key)
                                .map(|prev| (prev.timestamp_ms / BUCKET_MS) != bucket_id)
                                .unwrap_or(true);
                            if replace {
                                telemetry_latest_by_type.insert(key, pkt);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // On lag, keep going and send most recent snapshots at next flush.
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }

                _ = telemetry_flush.tick() => {
                    if telemetry_latest_by_type.is_empty() {
                        continue;
                    }

                    let mut rows: Vec<TelemetryRow> =
                        telemetry_latest_by_type.drain().map(|(_, row)| row).collect();
                    rows.sort_by_key(|r| r.timestamp_ms);

                    if rows.len() > MAX_TELEMETRY_PER_FLUSH {
                        rows.drain(0..(rows.len() - MAX_TELEMETRY_PER_FLUSH));
                    }

                    for row in rows {
                        let msg = WsOutMsg::Telemetry(row);
                        let text = serde_json::to_string(&msg).unwrap_or_default();
                        if sender
                            .send(Message::Text(Utf8Bytes::from(text)))
                            .await
                            .is_err()
                        {
                            break;
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
