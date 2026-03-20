use crate::gpio::GpioPins;
use crate::loadcell::LoadcellCalibrationFile;
use crate::ring_buffer::RingBuffer;
use crate::sequences::{ActionPolicyMsg, PersistentNotification, command_name};
use crate::web::{ErrorMsg, FlightStateMsg, WarningMsg};
use groundstation_shared::{
    Board, BoardStatusEntry, BoardStatusMsg, FlightState, TelemetryCommand, TelemetryRow,
};
use sedsprintf_rs_2026::config::DataEndpoint;
use sedsprintf_rs_2026::discovery::TopologySnapshot;
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::Router;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct BoardStatus {
    pub last_seen_ms: Option<u64>,
    pub ema_gap_ms: Option<u64>,
    pub warned: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTopologyNodeKind {
    Router,
    Endpoint,
    Side,
    Board,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTopologyStatus {
    Online,
    Offline,
    Simulated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkTopologyNode {
    pub id: String,
    pub label: String,
    pub kind: NetworkTopologyNodeKind,
    pub status: NetworkTopologyStatus,
    pub group: String,
    pub sender_id: Option<String>,
    pub endpoints: Vec<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkTopologyLink {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
    pub status: NetworkTopologyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkTopologyMsg {
    pub generated_ms: u64,
    pub simulated: bool,
    pub nodes: Vec<NetworkTopologyNode>,
    pub links: Vec<NetworkTopologyLink>,
}

#[derive(Clone)]
pub struct AppState {
    /// Optional ring buffer for full telemetry packets (not JSON)
    pub ring_buffer: Arc<Mutex<RingBuffer<Packet>>>,

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

    /// Latest calibrated fill-mass estimate from 1000kg loadcell.
    pub latest_fill_mass_kg: Arc<Mutex<Option<f32>>>,

    /// Loadcell calibration data loaded from JSON and editable at runtime.
    pub loadcell_calibration: Arc<Mutex<LoadcellCalibrationFile>>,

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

    /// In-memory recent telemetry cache used to bridge DB write lag during reseed.
    pub recent_telemetry_cache: Arc<Mutex<VecDeque<TelemetryRow>>>,

    /// Whether the av-bay (rocket) radio link is physically present.
    pub av_bay_radio_connected: Arc<AtomicBool>,

    /// Whether the fill-system (umbilical) radio link is physically present.
    pub fill_radio_connected: Arc<AtomicBool>,

    /// Shared router handle used for exporting discovery topology.
    pub topology_router: Arc<OnceLock<Arc<Router>>>,
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

    pub fn network_topology_snapshot(&self, now_ms: u64) -> NetworkTopologyMsg {
        let simulated = cfg!(feature = "testing");
        let exported = self
            .topology_router
            .get()
            .map(|router| router.export_topology());
        let board_snapshot = self.board_status_snapshot(now_ms);

        let rocket_radio_online = simulated || self.av_bay_radio_connected.load(Ordering::Relaxed);
        let fill_radio_online = simulated || self.fill_radio_connected.load(Ordering::Relaxed);

        let radio_status = |online: bool| {
            if simulated {
                NetworkTopologyStatus::Simulated
            } else if online {
                NetworkTopologyStatus::Online
            } else {
                NetworkTopologyStatus::Offline
            }
        };

        let endpoints_for_side = |snapshot: Option<&TopologySnapshot>, side_name: &str| {
            let mut endpoints = snapshot
                .and_then(|snapshot| {
                    snapshot
                        .routes
                        .iter()
                        .find(|route| route.side_name == side_name)
                })
                .map(|route| {
                    route
                        .reachable_endpoints
                        .iter()
                        .map(DataEndpoint::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            endpoints.sort();
            endpoints.dedup();
            endpoints
        };

        let expected_board_endpoints = |board: Board| -> Vec<String> {
            let endpoints = match board {
                Board::FlightComputer => {
                    &[DataEndpoint::FlightController, DataEndpoint::FlightState][..]
                }
                Board::ValveBoard => &[DataEndpoint::ValveBoard, DataEndpoint::Abort][..],
                Board::ActuatorBoard => &[DataEndpoint::ActuatorBoard, DataEndpoint::Abort][..],
                _ => &[][..],
            };
            endpoints
                .iter()
                .map(|endpoint| endpoint.as_str().to_string())
                .collect()
        };

        let board_endpoints =
            |board: Board, side_endpoints: &[String], simulated: bool| -> Vec<String> {
                let mut expected = expected_board_endpoints(board);
                if simulated {
                    return expected;
                }
                expected.retain(|endpoint| side_endpoints.iter().any(|side| side == endpoint));
                expected
            };

        let board_status = |board: Board| -> (NetworkTopologyStatus, Option<String>) {
            let Some(entry) = board_snapshot
                .boards
                .iter()
                .find(|entry| entry.board == board)
            else {
                return (NetworkTopologyStatus::Offline, None);
            };

            if entry.seen {
                let detail = entry
                    .age_ms
                    .map(|age_ms| format!("Last packet {} ms ago", age_ms));
                (NetworkTopologyStatus::Online, detail)
            } else if simulated {
                (
                    NetworkTopologyStatus::Simulated,
                    Some("Simulated network node".to_string()),
                )
            } else {
                (
                    NetworkTopologyStatus::Offline,
                    Some("No packets received".to_string()),
                )
            }
        };

        let side_status = |side_name: &str, default_online: bool| {
            if exported.as_ref().is_some_and(|snapshot| {
                snapshot
                    .routes
                    .iter()
                    .any(|route| route.side_name == side_name)
            }) {
                if simulated {
                    NetworkTopologyStatus::Simulated
                } else {
                    NetworkTopologyStatus::Online
                }
            } else {
                radio_status(default_online)
            }
        };

        let local_endpoint_list = exported
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .advertised_endpoints
                    .iter()
                    .map(DataEndpoint::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rocket_side_endpoints = endpoints_for_side(exported.as_ref(), "rocket_radio");
        let fill_side_endpoints = endpoints_for_side(exported.as_ref(), "umbilical_radio");

        let mut nodes = vec![
            NetworkTopologyNode {
                id: "router".to_string(),
                label: "Ground Station Router".to_string(),
                kind: NetworkTopologyNodeKind::Router,
                status: NetworkTopologyStatus::Online,
                group: "local".to_string(),
                sender_id: Some(Board::GroundStation.sender_id().to_string()),
                endpoints: local_endpoint_list.clone(),
                detail: Some("SEDSprintf relay router".to_string()),
            },
            NetworkTopologyNode {
                id: "side_rocket_radio".to_string(),
                label: "Rocket Radio Side".to_string(),
                kind: NetworkTopologyNodeKind::Side,
                status: side_status("rocket_radio", rocket_radio_online),
                group: "local".to_string(),
                sender_id: Some(Board::GroundStation.sender_id().to_string()),
                endpoints: rocket_side_endpoints.clone(),
                detail: Some(if rocket_side_endpoints.is_empty() {
                    "Ground-station rocket-side router interface".to_string()
                } else {
                    format!(
                        "Ground-station rocket-side route reaches: {}",
                        rocket_side_endpoints.join(", ")
                    )
                }),
            },
            NetworkTopologyNode {
                id: "side_umbilical_radio".to_string(),
                label: "Umbilical Radio Side".to_string(),
                kind: NetworkTopologyNodeKind::Side,
                status: side_status("umbilical_radio", fill_radio_online),
                group: "local".to_string(),
                sender_id: Some(Board::GroundStation.sender_id().to_string()),
                endpoints: fill_side_endpoints.clone(),
                detail: Some(if fill_side_endpoints.is_empty() {
                    "Ground-station fill-side router interface".to_string()
                } else {
                    format!(
                        "Ground-station fill-side route reaches: {}",
                        fill_side_endpoints.join(", ")
                    )
                }),
            },
        ];

        let rocket_boards = [Board::FlightComputer, Board::RFBoard, Board::PowerBoard];
        let fill_boards = [
            Board::ValveBoard,
            Board::ActuatorBoard,
            Board::GatewayBoard,
            Board::DaqBoard,
        ];

        for board in rocket_boards {
            let (status, detail) = board_status(board);
            let endpoints = board_endpoints(board, &rocket_side_endpoints, simulated);
            nodes.push(NetworkTopologyNode {
                id: format!("board_{}", board.sender_id().to_ascii_lowercase()),
                label: board.as_str().to_string(),
                kind: NetworkTopologyNodeKind::Board,
                status,
                group: "rocket_remote".to_string(),
                sender_id: Some(board.sender_id().to_string()),
                endpoints: endpoints.clone(),
                detail: detail.or_else(|| {
                    if endpoints.is_empty() {
                        None
                    } else {
                        Some(format!(
                            "Endpoints reachable on this board: {}",
                            endpoints.join(", ")
                        ))
                    }
                }),
            });
        }

        for board in fill_boards {
            let (status, detail) = board_status(board);
            let endpoints = board_endpoints(board, &fill_side_endpoints, simulated);
            nodes.push(NetworkTopologyNode {
                id: format!("board_{}", board.sender_id().to_ascii_lowercase()),
                label: board.as_str().to_string(),
                kind: NetworkTopologyNodeKind::Board,
                status,
                group: "fill_remote".to_string(),
                sender_id: Some(board.sender_id().to_string()),
                endpoints: endpoints.clone(),
                detail: detail.or_else(|| {
                    if endpoints.is_empty() {
                        None
                    } else {
                        Some(format!(
                            "Endpoints reachable on this board: {}",
                            endpoints.join(", ")
                        ))
                    }
                }),
            });
        }

        let mut links = vec![
            NetworkTopologyLink {
                source: "router".to_string(),
                target: "side_rocket_radio".to_string(),
                label: Some("rocket radio".to_string()),
                status: side_status("rocket_radio", rocket_radio_online),
            },
            NetworkTopologyLink {
                source: "router".to_string(),
                target: "side_umbilical_radio".to_string(),
                label: Some("umbilical radio".to_string()),
                status: side_status("umbilical_radio", fill_radio_online),
            },
            NetworkTopologyLink {
                source: "side_rocket_radio".to_string(),
                target: "board_rf".to_string(),
                label: Some("radio modem".to_string()),
                status: side_status("rocket_radio", rocket_radio_online),
            },
            NetworkTopologyLink {
                source: "side_umbilical_radio".to_string(),
                target: "board_gw".to_string(),
                label: Some("radio modem".to_string()),
                status: side_status("umbilical_radio", fill_radio_online),
            },
        ];

        for board in rocket_boards {
            let (status, _) = board_status(board);
            if matches!(board, Board::RFBoard) {
                continue;
            }
            links.push(NetworkTopologyLink {
                source: "board_rf".to_string(),
                target: format!("board_{}", board.sender_id().to_ascii_lowercase()),
                label: Some("physical link".to_string()),
                status,
            });
        }

        for board in fill_boards {
            let (status, _) = board_status(board);
            if matches!(board, Board::GatewayBoard) {
                continue;
            }
            links.push(NetworkTopologyLink {
                source: "board_gw".to_string(),
                target: format!("board_{}", board.sender_id().to_ascii_lowercase()),
                label: Some("physical link".to_string()),
                status,
            });
        }

        NetworkTopologyMsg {
            generated_ms: now_ms,
            simulated,
            nodes,
            links,
        }
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

    pub fn set_local_flight_state(&self, next_state: FlightState) {
        let mut slot = self.state.lock().unwrap();
        if *slot == next_state {
            return;
        }
        *slot = next_state;
        drop(slot);

        let _ = self.state_tx.send(FlightStateMsg { state: next_state });

        self.begin_db_write();
        let db = self.db.clone();
        let state_for_task = self.clone();
        let ts_ms = crate::telemetry_task::get_current_timestamp_ms() as i64;
        tokio::spawn(async move {
            let _ = sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                .bind(ts_ms)
                .bind(next_state as i64)
                .execute(&db)
                .await;
            state_for_task.end_db_write();
        });
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
        self.add_notification_with_persistence(message, true)
    }

    pub fn add_temporary_notification<S: Into<String>>(&self, message: S) -> u64 {
        self.add_notification_with_persistence(message, false)
    }

    pub fn add_notification_with_persistence<S: Into<String>>(
        &self,
        message: S,
        persistent: bool,
    ) -> u64 {
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
            persistent,
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
        if matches!(
            cmd,
            TelemetryCommand::Abort
                | TelemetryCommand::NitrogenClose
                | TelemetryCommand::NitrousClose
        ) {
            return true;
        }
        let name = command_name(cmd);
        let policy = self.action_policy.lock().unwrap();
        if !policy.software_buttons_enabled {
            return false;
        }
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

    pub fn cache_recent_telemetry(&self, row: TelemetryRow) {
        const CACHE_WINDOW_MS: i64 = 20 * 60 * 1000;
        const CACHE_MAX_ROWS: usize = 250_000;

        let mut q = self.recent_telemetry_cache.lock().unwrap();
        q.push_back(row);

        let newest_ts = q.back().map(|r| r.timestamp_ms).unwrap_or(0);
        let cutoff = newest_ts.saturating_sub(CACHE_WINDOW_MS);
        while let Some(front) = q.front() {
            if front.timestamp_ms < cutoff {
                q.pop_front();
            } else {
                break;
            }
        }

        while q.len() > CACHE_MAX_ROWS {
            q.pop_front();
        }
    }

    pub fn recent_telemetry_snapshot(&self) -> Vec<TelemetryRow> {
        self.recent_telemetry_cache
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }
}
