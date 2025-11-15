use crate::ring_buffer::RingBuffer;
use groundstation_shared::{TelemetryCommand, TelemetryRow};
use sqlx::SqlitePool;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;

#[derive(Clone)]
pub struct AppState {
    pub ring_buffer: Arc<Mutex<RingBuffer<TelemetryPacket>>>,
    pub cmd_tx: mpsc::Sender<TelemetryCommand>,
    pub ws_tx: broadcast::Sender<TelemetryRow>,
    pub db: SqlitePool,
}
