use crate::gpio::GpioPins;
use crate::ring_buffer::RingBuffer;
use crate::sequences::{command_name, ActionPolicyMsg, PersistentNotification};
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
    pub ema_gap_ms: Option<u64>,
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

    /// Persistent operator notifications shown to all clients until dismissed.
    pub notifications: Arc<Mutex<Vec<PersistentNotification>>>,

    /// Broadcast whenever notifications list changes.
    pub notifications_tx: broadcast::Sender<Vec<PersistentNotification>>,

    /// Monotonic ID source for notifications.
    pub next_notification_id: Arc<AtomicU64>,

    /// Current action policy (enabled/disabled/blink hints) for UI + command gating.
    pub action_policy: Arc<Mutex<ActionPolicyMsg>>,

    /// Broadcast whenever action policy changes.
    pub action_policy_tx: broadcast::Sender<ActionPolicyMsg>,

    /// Last accepted command timestamp by command name.
    pub last_command_ms: Arc<Mutex<HashMap<String, u64>>>,
}

impl AppState {
    pub fn mark_board_seen(&self, sender: &str, timestamp_ms: u64) {
        let Some(board) = Board::from_sender_id(sender) else {
            return;
        };
        let mut map = self.board_status.lock().unwrap();
        if let Some(status) = map.get_mut(&board) {
            if let Some(last_seen) = status.last_seen_ms {
                let gap_ms = timestamp_ms.saturating_sub(last_seen);
                let ema = status
                    .ema_gap_ms
                    .map(|prev| ((prev * 7) + gap_ms) / 8)
                    .unwrap_or(gap_ms);
                status.ema_gap_ms = Some(ema.clamp(10, 60_000));
            }
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

    pub fn notifications_snapshot(&self) -> Vec<PersistentNotification> {
        self.notifications.lock().unwrap().clone()
    }

    pub fn add_notification<S: Into<String>>(&self, message: S) -> u64 {
        let message = message.into();
        let mut notifications = self.notifications.lock().unwrap();
        if let Some(existing) = notifications.iter().find(|n| n.message == message) {
            return existing.id;
        }
        let id = self.next_notification_id.fetch_add(1, Ordering::Relaxed) + 1;
        notifications.push(PersistentNotification {
            id,
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
            message,
        });
        let snapshot = notifications.clone();
        drop(notifications);
        let _ = self.notifications_tx.send(snapshot);
        id
    }

    pub fn dismiss_notification(&self, id: u64) -> bool {
        let mut notifications = self.notifications.lock().unwrap();
        let before = notifications.len();
        notifications.retain(|n| n.id != id);
        if notifications.len() == before {
            return false;
        }
        let snapshot = notifications.clone();
        drop(notifications);
        let _ = self.notifications_tx.send(snapshot);
        true
    }

    pub fn action_policy_snapshot(&self) -> ActionPolicyMsg {
        self.action_policy.lock().unwrap().clone()
    }

    pub fn set_action_policy(&self, policy: ActionPolicyMsg) {
        let mut slot = self.action_policy.lock().unwrap();
        if *slot == policy {
            return;
        }
        *slot = policy.clone();
        drop(slot);
        let _ = self.action_policy_tx.send(policy);
    }

    pub fn is_command_allowed(&self, cmd: &TelemetryCommand) -> bool {
        if matches!(cmd, TelemetryCommand::Abort) {
            return true;
        }
        let name = command_name(cmd);
        let policy = self.action_policy.lock().unwrap();
        policy
            .controls
            .iter()
            .find(|c| c.cmd == name)
            .map(|c| c.enabled)
            .unwrap_or(false)
    }

    pub fn record_command_accepted(&self, cmd: &TelemetryCommand, ts_ms: u64) {
        let mut map = self.last_command_ms.lock().unwrap();
        map.insert(command_name(cmd).to_string(), ts_ms);
    }

    pub fn last_command_timestamp_ms(&self, cmd_name: &str) -> Option<u64> {
        self.last_command_ms.lock().unwrap().get(cmd_name).copied()
    }
}
