use crate::state::AppState;
use axum::http::header;
use axum::{
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade}, extract::{Query, State},
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use bytes::Bytes;
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use groundstation_shared::{FlightState, TelemetryCommand, TelemetryRow};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::OnceCell;
use tower_http::services::ServeDir;

static FAVICON_DATA: OnceCell<Bytes> = OnceCell::const_new();

/// Public router constructor
pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = ServeDir::new("./frontend/dist");

    Router::new()
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/history", get(get_history))
        .route("/api/alerts", get(get_alerts))
        .route("/favicon", get(get_favicon))
        .route("/flightstate", get(get_flight_state))
        .route("/ws", get(ws_handler))
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
}

#[derive(Clone, Serialize)]
pub struct FlightStateMsg {
    pub state: FlightState,
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

async fn get_recent(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let now_ms = Utc::now().timestamp_millis();
    let cutoff = now_ms - 20 * 60 * 1000; // 20 minutes

    let rows_db = sqlx::query(
        "SELECT timestamp_ms, data_type, v0, v1, v2, v3, v4, v5, v6, v7 \
         FROM telemetry \
         WHERE timestamp_ms >= ? \
         ORDER BY timestamp_ms ASC",
    )
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let rows: Vec<TelemetryRow> = rows_db
        .into_iter()
        .map(|row| TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            v0: row.get::<Option<f32>, _>("v0"),
            v1: row.get::<Option<f32>, _>("v1"),
            v2: row.get::<Option<f32>, _>("v2"),
            v3: row.get::<Option<f32>, _>("v3"),
            v4: row.get::<Option<f32>, _>("v4"),
            v5: row.get::<Option<f32>, _>("v5"),
            v6: row.get::<Option<f32>, _>("v6"),
            v7: row.get::<Option<f32>, _>("v7"),
        })
        .collect();

    Json(rows)
}

async fn get_favicon() -> impl IntoResponse {
    // Load the favicon into memory on first request, reuse later
    let bytes = FAVICON_DATA
        .get_or_init(|| async {
            // Adjust this path if needed
            let path: PathBuf = "./frontend/dist/favicon.png".into();
            let data = tokio::fs::read(&path)
                .await
                .unwrap_or_else(|e| panic!("failed to read favicon at {:?}: {e}", path));
            Bytes::from(data)
        })
        .await
        .clone();

    // Return as an image/png response
    ([(header::CONTENT_TYPE, "image/png")], bytes)
}
async fn get_flight_state(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // get the state from the db
    let data = sqlx::query("SELECT f_state FROM flight_state ORDER BY timestamp_ms DESC LIMIT 1")
        .fetch_one(&state.db)
        .await
        .expect("failed to fetch flight state");
    let flight_state: i64 = data.get::<i64, _>("f_state");
    let flight_state = groundstation_shared::u8_to_flight_state(flight_state as u8)
        .unwrap_or(groundstation_shared::FlightState::Startup);
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

    let cmd_tx = state.cmd_tx.clone();
    let (mut sender, mut receiver) = socket.split();

    // Task: server -> client (all streams multiplexed)
    let send_task = async move {
        loop {
            tokio::select! {
                Ok(pkt) = telemetry_rx.recv() => {
                    let msg = WsOutMsg::Telemetry(pkt);
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    if sender
                        .send(Message::Text(Utf8Bytes::from(text)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }

                Ok(fs) = state_rx.recv() => {
                    let msg  = WsOutMsg::FlightState(fs);
                    let text = serde_json::to_string(&msg).unwrap_or_default();
                    if sender.send(Message::Text(Utf8Bytes::from(text))).await.is_err() {
                        break;
                    }
                }

                Ok(warn) = warnings_rx.recv() => {
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

                Ok(err) = errors_rx.recv() => {
                    let msg = WsOutMsg::Error(err);
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

async fn get_history(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HistoryParams>,
) -> impl IntoResponse {
    let minutes = params.minutes.unwrap_or(20);
    let now_ms: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let cutoff = now_ms - (minutes as i64) * 60_000;

    let rows_db = sqlx::query(
        "SELECT timestamp_ms, data_type, v0, v1, v2, v3, v4, v5, v6, v7 \
         FROM telemetry \
         WHERE timestamp_ms >= ? \
         ORDER BY timestamp_ms ASC",
    )
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let rows: Vec<TelemetryRow> = rows_db
        .into_iter()
        .map(|row| TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type: row.get::<String, _>("data_type"),
            v0: row.get::<Option<f32>, _>("v0"),
            v1: row.get::<Option<f32>, _>("v1"),
            v2: row.get::<Option<f32>, _>("v2"),
            v3: row.get::<Option<f32>, _>("v3"),
            v4: row.get::<Option<f32>, _>("v4"),
            v5: row.get::<Option<f32>, _>("v5"),
            v6: row.get::<Option<f32>, _>("v6"),
            v7: row.get::<Option<f32>, _>("v7"),
        })
        .collect();

    Json(rows)
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

/// Helper: current timestamp in ms (i64) for warnings/errors/etc.
fn now_ms_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

    // 2) Insert into DB asynchronously
    let db = state.db.clone();
    tokio::spawn(async move {
        let _ = sqlx::query(
            r#"
            INSERT INTO alerts (timestamp_ms, severity, message)
            VALUES (?, 'warning', ?)
            "#,
        )
        .bind(timestamp)
        .bind(msg_string)
        .execute(&db)
        .await;
    });
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

    // 2) Insert into DB asynchronously
    let db = state.db.clone();
    tokio::spawn(async move {
        let _ = sqlx::query(
            r#"
            INSERT INTO alerts (timestamp_ms, severity, message)
            VALUES (?, 'error', ?)
            "#,
        )
        .bind(timestamp)
        .bind(msg_string)
        .execute(&db)
        .await;
    });
}
