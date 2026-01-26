use crate::gpio::GpioPins;
use crate::ring_buffer::RingBuffer;
use crate::web::{ErrorMsg, FlightStateMsg, WarningMsg};
use groundstation_shared::{Board, BoardStatusEntry, BoardStatusMsg, FlightState, TelemetryCommand, TelemetryRow};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone)]
pub struct BoardStatus {
    pub last_seen_ms: Option<u64>,
    pub warned: bool,
}

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

    /// Current flight state
    pub state: Arc<Mutex<FlightState>>,

    /// Flight state updates → frontend
    pub state_tx: broadcast::Sender<FlightStateMsg>,

    /// GPIO interface
    pub gpio: Arc<GpioPins>,

    /// Board heartbeat status
    pub board_status: Arc<Mutex<HashMap<Board, BoardStatus>>>,

    /// Board status updates → frontend
    pub board_status_tx: broadcast::Sender<BoardStatusMsg>,
}

impl AppState {
    pub fn mark_board_seen(&self, sender: &str, timestamp_ms: u64) {
        let Some(board) = Board::from_sender_id(sender) else {
            return;
        };
        let mut map = self.board_status.lock().unwrap();
        if let Some(status) = map.get_mut(&board) {
            status.last_seen_ms = Some(timestamp_ms);
            status.warned = false;
        }
    }

    pub fn board_status_snapshot(&self, now_ms: u64) -> BoardStatusMsg {
        let map = self.board_status.lock().unwrap();
        let mut boards = Vec::with_capacity(Board::ALL.len());

        for board in Board::ALL {
            let status = map.get(board);
            let last_seen_ms = status.and_then(|s| s.last_seen_ms);
            let age_ms = last_seen_ms.map(|ts| now_ms.saturating_sub(ts));
            let seen = last_seen_ms.is_some();

            boards.push(BoardStatusEntry {
                board: *board,
                sender_id: board.sender_id().to_string(),
                seen,
                last_seen_ms,
                age_ms,
            });
        }

        BoardStatusMsg { boards }
    }
}
