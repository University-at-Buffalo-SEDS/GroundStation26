use crate::ring_buffer::RingBuffer;
use groundstation_shared::{TelemetryCommand, TelemetryRow};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sqlx::SqlitePool;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

use crate::web::{ErrorMsg, WarningMsg};

#[derive(Clone)]
pub struct AppState {
    /// Optional ring buffer for full telemetry packets (not JSON)
    pub ring_buffer: Arc<Mutex<RingBuffer<TelemetryPacket>>>,

    /// Commands from frontend → server (Arm, Disarm, Abort, etc.)
    pub cmd_tx: mpsc::Sender<TelemetryCommand>,

    /// Telemetry stream → frontend
    pub ws_tx: broadcast::Sender<TelemetryRow>,

    /// Warning messages → frontend
    pub warnings_tx: broadcast::Sender<WarningMsg>,

    /// Error messages → frontend
    pub errors_tx: broadcast::Sender<ErrorMsg>,

    /// SQLite database
    pub db: SqlitePool,
}
