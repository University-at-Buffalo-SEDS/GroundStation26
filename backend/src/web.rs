use crate::state::AppState;
use axum::{
    extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade}, extract::{Query, State},
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use groundstation_shared::{TelemetryCommand, TelemetryRow};
use serde::Deserialize;
use sqlx::Row;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tower_http::services::ServeDir;

pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = ServeDir::new("./frontend/dist");

    Router::new()
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/history", get(get_history))
        .route("/ws", get(ws_handler))
        // anything that doesn’t match the above routes goes to the static files
        .fallback_service(static_dir)
        .with_state(state)
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
    let mut rx = state.ws_tx.subscribe();
    let cmd_tx = state.cmd_tx.clone();

    let (mut sender, mut receiver) = socket.split();

    // Task: server -> client (telemetry stream)
    let send_task = async move {
        while let Ok(pkt) = rx.recv().await {
            let text = serde_json::to_string(&pkt).unwrap_or_default();
            if sender
                .send(Message::Text(Utf8Bytes::from(text)))
                .await
                .is_err()
            {
                break;
            }
        }
    };

    // Task: client -> server (commands)
    let recv_task = async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                match serde_json::from_str::<WsCommand>(&text) {
                    Ok(cmd) => {
                        // Forward to the same command channel used by /api/command
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
        "SELECT timestamp_ms, data_type, v0, v1, v2 \
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
