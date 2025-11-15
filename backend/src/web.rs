use crate::state::AppState;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use groundstation_shared::{TelemetryCommand, TelemetryRow};

use axum::extract::ws::Utf8Bytes;
use sqlx::Row;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use axum::extract::Query;
use tower_http::services::ServeDir;
use serde::Deserialize;

pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = ServeDir::new("../frontend/dist");

    Router::new()
        .route("/api/recent", get(get_recent))
        .route("/api/command", post(send_command))
        .route("/api/history", get(get_history))   // <- NEW
        .route("/ws", get(ws_handler))
        // anything that doesn’t match the above routes goes to the static files
        .fallback_service(static_dir)
        .with_state(state)
}


async fn get_recent(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let now_ms = Utc::now().timestamp_millis();
    let cutoff = now_ms - 20 * 60 * 1000; // 20 minutes

    let rows_db = sqlx::query(
        "SELECT timestamp_ms, data_type, v0, v1, v2 \
         FROM telemetry \
         WHERE timestamp_ms >= ? \
         ORDER BY timestamp_ms ASC"
    )
    .bind(cutoff)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let rows: Vec<TelemetryRow> = rows_db
        .into_iter()
        .map(|row| TelemetryRow {
            timestamp_ms: row.get::<i64, _>("timestamp_ms"),
            data_type:    row.get::<String, _>("data_type"),
            v0:           row.get::<Option<f32>, _>("v0"),
            v1:           row.get::<Option<f32>, _>("v1"),
            v2:           row.get::<Option<f32>, _>("v2"),
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.ws_tx.subscribe();

    // We only push server → client packets in this example.
    while let Ok(pkt) = rx.recv().await {
        let text = serde_json::to_string(&pkt).unwrap_or_default();
        if socket.send(Message::Text(Utf8Bytes::from(text))).await.is_err() {
            break;
        }
    }
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
         ORDER BY timestamp_ms ASC"
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
        })
        .collect();

    Json(rows)
}
