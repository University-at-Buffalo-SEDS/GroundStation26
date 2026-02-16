use crate::gpio::GpioPins;
use crate::ring_buffer::RingBuffer;
use crate::web::{ErrorMsg, FlightStateMsg, WarningMsg};
use groundstation_shared::{
    Board, BoardStatusEntry, BoardStatusMsg, FlightState, TelemetryCommand, TelemetryRow,
};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::time::{Duration, Instant};

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

    /// Last time (ms) any telemetry packet was received by the router.
    pub last_packet_rx_ms: Arc<AtomicU64>,

    /// Umbilical valve states keyed by command id (u8)
    pub umbilical_valve_states: Arc<Mutex<HashMap<u8, bool>>>,

    /// Latest fuel tank pressure (psi)
    pub latest_fuel_tank_pressure: Arc<Mutex<Option<f32>>>,

    /// Broadcast shutdown notifications to long-running background tasks.
    pub shutdown_tx: broadcast::Sender<()>,

    /// Number of in-flight async DB writes (alerts/warnings/errors).
    pub pending_db_writes: Arc<AtomicUsize>,

    /// Notifies waiters when pending DB writes changes.
    pub db_write_notify: Arc<Notify>,
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

    pub fn all_boards_seen(&self) -> bool {
        let map = self.board_status.lock().unwrap();
        map.values().all(|status| status.last_seen_ms.is_some())
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

    pub fn set_umbilical_valve_state(&self, cmd_id: u8, on: bool) {
        let mut map = self.umbilical_valve_states.lock().unwrap();
        map.insert(cmd_id, on);
    }

    pub fn mark_packet_received(&self, timestamp_ms: u64) {
        self.last_packet_rx_ms
            .store(timestamp_ms, Ordering::Relaxed);
    }

    pub fn last_packet_received_ms(&self) -> u64 {
        self.last_packet_rx_ms.load(Ordering::Relaxed)
    }

    pub fn get_umbilical_valve_state(&self, cmd_id: u8) -> Option<bool> {
        let map = self.umbilical_valve_states.lock().unwrap();
        map.get(&cmd_id).copied()
    }

    pub fn shutdown_subscribe(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    pub fn begin_db_write(&self) {
        self.pending_db_writes.fetch_add(1, Ordering::SeqCst);
    }

    pub fn end_db_write(&self) {
        if self.pending_db_writes.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.db_write_notify.notify_waiters();
        }
    }

    pub fn pending_db_write_count(&self) -> usize {
        self.pending_db_writes.load(Ordering::SeqCst)
    }

    pub async fn wait_for_db_writes(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.pending_db_write_count() == 0 {
                return true;
            }

            let now = Instant::now();
            if now >= deadline {
                return false;
            }

            let remaining = deadline.saturating_duration_since(now);
            if tokio::time::timeout(remaining, self.db_write_notify.notified())
                .await
                .is_err()
            {
                return self.pending_db_write_count() == 0;
            }
        }
    }
}
