mod ring_buffer;
mod safety_task;
mod state;
mod telemetry_task;
mod web;
mod telemetry_decode;

use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::state::AppState;
use crate::telemetry_task::telemetry_task;
use crate::web::router;

use axum::Router;
use std::path::Path;
use std::fs;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // --- DB path ---
     let db_path = "groundstation.db";

    if !Path::new(db_path).exists() {
        // Create an empty file. That's it.
        fs::write(db_path, b"")?;
        println!("Created empty DB file.");
    }

    // Now SQLx can open it
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path)).await?;

    // Create the telemetry table if it doesn't exist
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
            v0           REAL,
            v1           REAL,
            v2           REAL
        );
        "#,
    )
    .execute(&db)
    .await?;

    // --- Channels ---
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (ws_tx, _ws_rx) = broadcast::channel(512);

    // --- Shared state ---
    let state = Arc::new(AppState {
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(2048))),
        cmd_tx,
        ws_tx,
        db,
    });

    // --- Background tasks ---
    tokio::spawn(telemetry_task(state.clone(), cmd_rx));
    tokio::spawn(safety_task(state.clone()));

    // --- Webserver ---
    let app: Router = router(state);

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
