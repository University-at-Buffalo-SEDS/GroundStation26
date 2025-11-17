mod ring_buffer;
mod safety_task;
mod state;
mod telemetry_decode;
mod telemetry_task;
mod web;

mod dummy_packets;
mod radio;

use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::state::AppState;
use crate::telemetry_task::{get_current_timestamp_ms, telemetry_task};

use crate::radio::{DummyRadio, Radio, RadioDevice, RADIO_BAUDRATE, RADIO_PORT};
use axum::Router;
use sedsprintf_rs_2026::config::DataEndpoint::{GroundStation, Abort};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::EndpointHandler;
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::{TelemetryError, TelemetryResult};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing_subscriber::EnvFilter;

fn clock() -> Box<dyn sedsprintf_rs_2026::router::Clock + Send + Sync> {
    Box::new(|| get_current_timestamp_ms())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- DB path ---
    let db_path = "./data/groundstation.db";

    if !Path::new(db_path).exists() {
        // Create an empty file. That's it.
        fs::write(db_path, b"")?;
        println!("Created empty DB file.");
    }
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path)).await?;
    // --- Channels ---
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (ws_tx, _ws_rx) = broadcast::channel(512);
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
            v0           REAL,
            v1           REAL,
            v2           REAL,
            v3           REAL,
            v4           REAL,
            v5           REAL,
            v6           REAL,
            v7           REAL

        );
        "#,
    )
    .execute(&db)
    .await?;
    // --- Shared state ---
    let state = Arc::new(AppState {
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(2048))),
        cmd_tx,
        ws_tx,
        db,
    });

    let ground_station_handler_state_clone = state.clone();


    let ground_station_handler =
        EndpointHandler::new_packet_handler(GroundStation, move |pkt: &TelemetryPacket| {
            let mut rb = ground_station_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(pkt.clone());
            Ok(())
        });
    let abort_handler =
        EndpointHandler::new_packet_handler(Abort, move |_pkt: &TelemetryPacket| {
            println!("Abort packet received!");
            Ok(())
        });
    let cfg = sedsprintf_rs_2026::router::BoardConfig::new([ground_station_handler, abort_handler]);
    let radio: Arc<Mutex<Box<dyn RadioDevice>>> = match Radio::open(RADIO_PORT, RADIO_BAUDRATE) {
        Ok(r) => {
            tracing::info!("Radio online");
            Arc::new(Mutex::new(Box::new(r)))
        }
        Err(e) => {
            tracing::warn!("Radio missing, using DummyRadio: {}", e);
            Arc::new(Mutex::new(Box::new(DummyRadio::new())))
        }
    };
    let serialized_handler = {
        let radio = Arc::clone(&radio);
        Some(move |pkt: &[u8]| -> TelemetryResult<()> {
            let mut guard = radio
                .lock()
                .map_err(|_| TelemetryError::HandlerError("Radio mutex poisoned"))?;
            guard
                .send_data(pkt)
                .map_err(|_| TelemetryError::HandlerError("Tx Handler failed"))?;
            Ok(())
        })
    };
    let router = Arc::new(sedsprintf_rs_2026::router::Router::new(
        serialized_handler,
        cfg,
        clock(),
    ));
    router.log_queue(DataType::MessageData, "hello".as_bytes())?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // --- Background tasks ---
    let _tt = tokio::spawn(telemetry_task(state.clone(), router.clone(), radio, cmd_rx));
    let _st = tokio::spawn(safety_task(state.clone(), router.clone()));

    // --- Webserver ---
    let app: Router = web::router(state);

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
