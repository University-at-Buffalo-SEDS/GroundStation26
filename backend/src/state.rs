use crate::auth::AuthManager;
use crate::gpio::GpioPins;
use crate::loadcell::LoadcellCalibrationFile;
use crate::ring_buffer::RingBuffer;
use crate::sequences::{ActionPolicyMsg, PersistentNotification, command_name};
use crate::types::{
    Board, BoardStatusEntry, BoardStatusMsg, FlightState, NetworkTopologyLink, NetworkTopologyMsg,
    NetworkTopologyNode, NetworkTopologyNodeKind, NetworkTopologyStatus, TelemetryCommand,
    TelemetryRow,
};
use crate::web::{AlertDto, ErrorMsg, FlightStateMsg, WarningMsg};
use sedsprintf_rs_2026::config::DataEndpoint;
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::Router;
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

    /// Main telemetry/application SQLite database.
    pub db: SqlitePool,

    /// Separate SQLite database for auth sessions.
    pub auth_db: SqlitePool,

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

    /// Monotonic counter for operator requests to continue a failed fill sequence.
    pub fill_sequence_continue_requests: Arc<AtomicU64>,

    /// In-memory recent telemetry cache used to bridge DB write lag during reseed.
    pub recent_telemetry_cache: Arc<Mutex<VecDeque<TelemetryRow>>>,

    /// Latest raw GPS fix values keyed by sender ID.
    pub latest_gps_fix_by_sender: Arc<Mutex<HashMap<String, Vec<Option<f32>>>>>,

    /// Latest GPS satellite count keyed by sender ID.
    pub latest_gps_satellites_by_sender: Arc<Mutex<HashMap<String, u8>>>,

    /// In-memory recent alerts cache used to bridge DB write lag during reseed.
    pub recent_alerts_cache: Arc<Mutex<VecDeque<AlertDto>>>,

    /// Whether the av-bay (rocket) comms link is physically present.
    pub av_bay_comms_connected: Arc<AtomicBool>,

    /// Whether the fill-system (umbilical) comms link is physically present.
    pub fill_comms_connected: Arc<AtomicBool>,

    /// Shared router handle used for exporting discovery topology.
    pub topology_router: Arc<OnceLock<Arc<Router>>>,

    /// Authentication and authorization manager.
    pub auth: Arc<AuthManager>,
}

impl AppState {
    fn layout_expected_boards(&self) -> std::collections::HashSet<Board> {
        let configured = crate::layout::load_layout()
            .ok()
            .map(|layout| {
                layout
                    .network_tab
                    .expected_boards
                    .into_iter()
                    .filter_map(|sender_id| Board::from_sender_id(sender_id.trim()))
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        if configured.is_empty() {
            Board::ALL
                .iter()
                .copied()
                .filter(|board| *board != Board::GroundStation)
                .collect()
        } else {
            configured
        }
    }

    /// Updates heartbeat tracking for a board after a packet arrives from that sender.
    pub fn mark_board_seen(&self, sender: &str, timestamp_ms: u64) {
        let Some(board) = Board::from_sender_id(sender) else {
            return;
        };
        let mut map = self.board_status.lock().unwrap();
        if let Some(status) = map.get_mut(&board) {
            if let Some(last_seen) = status.last_seen_ms {
                let gap_ms = timestamp_ms.saturating_sub(last_seen);
                // Smooth the inter-packet gap so the UI can reason about board health
                // without reacting to every short burst or stall.
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

    /// Returns whether every known board has been observed at least once.
    #[allow(dead_code)]
    pub fn all_boards_seen(&self) -> bool {
        let map = self.board_status.lock().unwrap();
        map.values().all(|status| status.last_seen_ms.is_some())
    }

    /// Returns whether a board should be required for startup/state gating in the current mode.
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    pub fn board_required_for_progression(&self, board: Board) -> bool {
        if !self.layout_expected_boards().contains(&board) {
            return false;
        }
        let av_link_up = self.av_bay_comms_connected.load(Ordering::Relaxed);
        let fill_link_up = self.fill_comms_connected.load(Ordering::Relaxed);
        if !av_link_up
            && matches!(
                board,
                Board::FlightComputer | Board::RFBoard | Board::PowerBoard | Board::DaqBoard
            )
        {
            return false;
        }
        if !fill_link_up
            && matches!(
                board,
                Board::ValveBoard | Board::ActuatorBoard | Board::GatewayBoard
            )
        {
            return false;
        }

        true
    }

    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
    pub fn board_required_for_progression(&self, board: Board) -> bool {
        self.layout_expected_boards().contains(&board)
    }

    /// Returns whether every board required for the current operating mode has been observed.
    pub fn all_required_boards_seen(&self) -> bool {
        let map = self.board_status.lock().unwrap();
        Board::ALL.iter().all(|board| {
            if !self.board_required_for_progression(*board) {
                return true;
            }
            map.get(board)
                .and_then(|status| status.last_seen_ms)
                .is_some()
        })
    }

    /// Builds the board-health payload sent to the dashboard.
    pub fn board_status_snapshot(&self, now_ms: u64) -> BoardStatusMsg {
        let map = self.board_status.lock().unwrap();
        let mut boards = Vec::with_capacity(Board::ALL.len());

        for board in Board::ALL {
            let status = map.get(board);
            let last_seen_ms = status.and_then(|s| s.last_seen_ms);
            let seen = last_seen_ms.is_some();
            if !seen {
                continue;
            }
            let age_ms = last_seen_ms.map(|ts| now_ms.saturating_sub(ts));

            boards.push(BoardStatusEntry {
                board: *board,
                board_label: board.as_str().to_string(),
                sender_id: board.sender_id().to_string(),
                seen,
                last_seen_ms,
                age_ms,
            });
        }

        BoardStatusMsg { boards }
    }

    /// Projects the current router and board state into the UI-friendly topology graph.
    pub fn network_topology_snapshot(&self, now_ms: u64) -> NetworkTopologyMsg {
        let simulated = cfg!(feature = "testing");
        let exported = if cfg!(feature = "test_fire_mode") {
            // In test-fire mode the AV-bay side is intentionally absent and both comms links may
            // be dummy placeholders. Avoid router topology export here; that path has proven
            // fragile with disconnected-link operator setups, while the dashboard can still render
            // a useful topology from board visibility and known side mappings alone.
            None
        } else {
            self.topology_router
                .get()
                .map(|router| router.export_topology())
        };
        let board_snapshot = self.board_status_snapshot(now_ms);
        let route_snapshot = exported.as_ref();
        let local_endpoint_list = route_snapshot
            .map(|snapshot| {
                let mut endpoints = snapshot
                    .advertised_endpoints
                    .iter()
                    .copied()
                    .map(|ep| ep.as_str())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                endpoints.sort();
                endpoints.dedup();
                endpoints
            })
            .unwrap_or_default();
        let local_visible_endpoint_list = local_endpoint_list
            .iter()
            .filter(|endpoint| endpoint.as_str() != DataEndpoint::GroundStation.as_str())
            .cloned()
            .collect::<Vec<_>>();
        let side_endpoints = |side_name: &str| {
            let mut endpoints = route_snapshot
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
                        .copied()
                        .map(|ep| ep.as_str().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            endpoints.sort();
            endpoints.dedup();
            endpoints
        };
        let rocket_side_endpoints = side_endpoints("rocket_comms");
        let fill_side_endpoints = side_endpoints("umbilical_comms");

        let mut nodes = vec![NetworkTopologyNode {
            id: "router".to_string(),
            label: "Ground Station Router".to_string(),
            kind: NetworkTopologyNodeKind::Router,
            status: NetworkTopologyStatus::Online,
            group: "local".to_string(),
            sender_id: Some(Board::GroundStation.sender_id().to_string()),
            endpoints: local_visible_endpoint_list.clone(),
            show_in_details: true,
            detail: Some("SEDSprintf relay router".to_string()),
        }];
        let mut links = Vec::new();
        let mut endpoint_ids = std::collections::BTreeSet::new();
        let mut side_ids = std::collections::BTreeMap::<String, String>::new();

        if let Some(snapshot) = route_snapshot {
            for route in &snapshot.routes {
                let side_id = format!("side_{}", route.side_name.to_ascii_lowercase());
                side_ids.insert(route.side_name.to_string(), side_id.clone());
                let mut endpoints = route
                    .reachable_endpoints
                    .iter()
                    .copied()
                    .map(|ep| ep.as_str())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                endpoints.sort();
                endpoints.dedup();
                let status = if simulated {
                    NetworkTopologyStatus::Simulated
                } else {
                    NetworkTopologyStatus::Online
                };
                nodes.push(NetworkTopologyNode {
                    id: side_id.clone(),
                    label: route.side_name.to_string(),
                    kind: NetworkTopologyNodeKind::Side,
                    status,
                    group: "side".to_string(),
                    sender_id: None,
                    endpoints: endpoints.clone(),
                    show_in_details: true,
                    detail: Some(format!("Last discovery route {} ms ago", route.age_ms)),
                });
                links.push(NetworkTopologyLink {
                    source: "router".to_string(),
                    target: side_id.clone(),
                    label: Some("route".to_string()),
                    status,
                });
                for endpoint in endpoints
                    .into_iter()
                    .filter(|endpoint| endpoint != DataEndpoint::GroundStation.as_str())
                {
                    let endpoint_id = format!("endpoint_{}", endpoint.to_ascii_lowercase());
                    if endpoint_ids.insert(endpoint_id.clone()) {
                        nodes.push(NetworkTopologyNode {
                            id: endpoint_id.clone(),
                            label: endpoint.clone(),
                            kind: NetworkTopologyNodeKind::Endpoint,
                            status,
                            group: "endpoint".to_string(),
                            sender_id: None,
                            endpoints: vec![endpoint.clone()],
                            show_in_details: true,
                            detail: None,
                        });
                    }
                    links.push(NetworkTopologyLink {
                        source: side_id.clone(),
                        target: endpoint_id,
                        label: Some("advertises".to_string()),
                        status,
                    });
                }
            }
        }

        for endpoint in &local_visible_endpoint_list {
            let endpoint_id = format!("endpoint_{}", endpoint.to_ascii_lowercase());
            if endpoint_ids.insert(endpoint_id.clone()) {
                nodes.push(NetworkTopologyNode {
                    id: endpoint_id.clone(),
                    label: endpoint.clone(),
                    kind: NetworkTopologyNodeKind::Endpoint,
                    status: NetworkTopologyStatus::Online,
                    group: "endpoint".to_string(),
                    sender_id: None,
                    endpoints: vec![endpoint.clone()],
                    show_in_details: true,
                    detail: Some("Locally advertised endpoint".to_string()),
                });
            }
            links.push(NetworkTopologyLink {
                source: "router".to_string(),
                target: endpoint_id,
                label: Some("local".to_string()),
                status: NetworkTopologyStatus::Online,
            });
        }

        let board_side = |board: Board| -> Option<&'static str> {
            match board {
                Board::GroundStation => None,
                Board::FlightComputer | Board::RFBoard | Board::PowerBoard => Some("rocket_comms"),
                Board::ValveBoard
                | Board::GatewayBoard
                | Board::ActuatorBoard
                | Board::DaqBoard => Some("umbilical_comms"),
            }
        };
        let side_relay = |side_name: &str| -> Option<Board> {
            match side_name {
                "rocket_comms" => Some(Board::RFBoard),
                "umbilical_comms" => Some(Board::GatewayBoard),
                _ => None,
            }
        };
        let seen_boards = board_snapshot
            .boards
            .iter()
            .filter(|entry| entry.seen && entry.board != Board::GroundStation)
            .map(|entry| {
                (
                    entry.board,
                    format!("board_{}", entry.sender_id.to_ascii_lowercase()),
                )
            })
            .collect::<HashMap<_, _>>();

        for entry in board_snapshot
            .boards
            .iter()
            .filter(|entry| entry.seen && entry.board != Board::GroundStation)
        {
            let node_id = format!("board_{}", entry.sender_id.to_ascii_lowercase());
            let status = if simulated {
                NetworkTopologyStatus::Simulated
            } else {
                NetworkTopologyStatus::Online
            };
            nodes.push(NetworkTopologyNode {
                id: node_id.clone(),
                label: entry.board_label.clone(),
                kind: NetworkTopologyNodeKind::Board,
                status,
                group: "board".to_string(),
                sender_id: Some(entry.sender_id.clone()),
                endpoints: modeled_board_endpoints(
                    entry.board,
                    simulated,
                    &local_visible_endpoint_list,
                    &rocket_side_endpoints,
                    &fill_side_endpoints,
                ),
                show_in_details: true,
                detail: entry
                    .age_ms
                    .map(|age_ms| format!("Last packet {} ms ago", age_ms)),
            });
            let source = if let Some(side_name) = board_side(entry.board) {
                if side_relay(side_name) == Some(entry.board) {
                    "router".to_string()
                } else if let Some(relay_id) =
                    side_relay(side_name).and_then(|relay| seen_boards.get(&relay))
                {
                    relay_id.clone()
                } else {
                    "router".to_string()
                }
            } else {
                "router".to_string()
            };

            links.push(NetworkTopologyLink {
                source,
                target: node_id,
                label: Some("seen".to_string()),
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

    /// Stores the most recent commanded umbilical valve state by command id.
    pub fn set_umbilical_valve_state(&self, cmd_id: u8, on: bool) {
        let mut map = self.umbilical_valve_states.lock().unwrap();
        map.insert(cmd_id, on);
    }

    /// Records when any telemetry packet last reached the backend router.
    pub fn mark_packet_received(&self, timestamp_ms: u64) {
        self.last_packet_rx_ms
            .store(timestamp_ms, Ordering::Relaxed);
    }

    /// Returns the timestamp of the most recent telemetry packet seen by the backend.
    pub fn last_packet_received_ms(&self) -> u64 {
        self.last_packet_rx_ms.load(Ordering::Relaxed)
    }

    /// Looks up the cached state for a specific umbilical valve command id.
    pub fn get_umbilical_valve_state(&self, cmd_id: u8) -> Option<bool> {
        let map = self.umbilical_valve_states.lock().unwrap();
        map.get(&cmd_id).copied()
    }

    /// Subscribes a task to the app-wide shutdown broadcast channel.
    pub fn shutdown_subscribe(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// Broadcasts a shutdown request to all long-running tasks.
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Increments the count of in-flight async database writes.
    pub fn begin_db_write(&self) {
        self.pending_db_writes.fetch_add(1, Ordering::SeqCst);
    }

    /// Updates local flight state, notifies subscribers, and persists the transition asynchronously.
    pub fn set_local_flight_state(&self, next_state: FlightState) {
        let mut slot = self.state.lock().unwrap();
        if *slot == next_state {
            return;
        }
        *slot = next_state;
        drop(slot);
        // Keep the simulator in lock-step with the real backend state machine.
        crate::flight_sim::sync_local_flight_state(next_state);

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

    /// Decrements the in-flight DB write count and wakes any shutdown waiters when it reaches zero.
    pub fn end_db_write(&self) {
        if self.pending_db_writes.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.db_write_notify.notify_waiters();
        }
    }

    /// Returns the number of async database writes still in progress.
    pub fn pending_db_write_count(&self) -> usize {
        self.pending_db_writes.load(Ordering::SeqCst)
    }

    /// Waits until all tracked async database writes have completed or the timeout expires.
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

    /// Clones the current notification list for HTTP or WebSocket consumers.
    pub fn notifications_snapshot(&self) -> Vec<PersistentNotification> {
        self.notifications.lock().unwrap().clone()
    }

    /// Adds a persistent operator notification and returns its assigned id.
    pub fn add_notification<S: Into<String>>(&self, message: S) -> u64 {
        self.add_notification_with_persistence(message, true)
    }

    /// Adds a transient operator notification and returns its assigned id.
    pub fn add_temporary_notification<S: Into<String>>(&self, message: S) -> u64 {
        self.add_notification_with_persistence(message, false)
    }

    /// Adds a notification while explicitly controlling whether it persists across reloads.
    pub fn add_notification_with_persistence<S: Into<String>>(
        &self,
        message: S,
        persistent: bool,
    ) -> u64 {
        self.add_notification_action(message, persistent, None, None)
    }

    /// Inserts a notification if an identical message is not already active.
    pub fn add_notification_action<S: Into<String>>(
        &self,
        message: S,
        persistent: bool,
        action_label: Option<String>,
        action_cmd: Option<String>,
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
            action_label,
            action_cmd,
        });
        let snapshot = notifications.clone();
        drop(notifications);
        let _ = self.notifications_tx.send(snapshot);
        id
    }

    /// Removes a notification by id and broadcasts the updated list.
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

    /// Returns the latest action-policy snapshot used by the dashboard and command gate.
    pub fn action_policy_snapshot(&self) -> ActionPolicyMsg {
        self.action_policy.lock().unwrap().clone()
    }

    /// Records an operator request to continue the fill sequence.
    pub fn request_fill_sequence_continue(&self) {
        self.fill_sequence_continue_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Consumes any queued fill-sequence continue requests and reports whether one existed.
    pub fn consume_fill_sequence_continue_requests(&self) -> bool {
        let prev = self
            .fill_sequence_continue_requests
            .swap(0, Ordering::Relaxed);
        prev > 0
    }

    /// Replaces the current action policy and broadcasts it if it changed.
    pub fn set_action_policy(&self, policy: ActionPolicyMsg) {
        let mut slot = self.action_policy.lock().unwrap();
        if *slot == policy {
            return;
        }
        *slot = policy.clone();
        drop(slot);
        let _ = self.action_policy_tx.send(policy);
    }

    /// Applies the current software-action policy to decide whether a command can run.
    pub fn is_command_allowed(&self, cmd: &TelemetryCommand) -> bool {
        if matches!(
            cmd,
            TelemetryCommand::Abort
                | TelemetryCommand::NitrogenClose
                | TelemetryCommand::NitrousClose
                | TelemetryCommand::ContinueFillSequence
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

    /// Records when a command was last accepted by the backend.
    pub fn record_command_accepted(&self, cmd: &TelemetryCommand, ts_ms: u64) {
        let mut map = self.last_command_ms.lock().unwrap();
        map.insert(command_name(cmd).to_string(), ts_ms);
    }

    /// Looks up the last accepted timestamp for a command name.
    pub fn last_command_timestamp_ms(&self, cmd_name: &str) -> Option<u64> {
        self.last_command_ms.lock().unwrap().get(cmd_name).copied()
    }

    /// Appends a telemetry row to the in-memory reseed cache and prunes old entries.
    pub fn cache_recent_telemetry(&self, row: TelemetryRow) {
        const CACHE_WINDOW_MS: i64 = 20 * 60 * 1000;
        const CACHE_MAX_ROWS: usize = 250_000;

        // The cache is a bridge for startup/reconnect reseeds, not a second source of truth.
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

    /// Returns a point-in-time clone of the recent telemetry cache.
    pub fn recent_telemetry_snapshot(&self) -> Vec<TelemetryRow> {
        self.recent_telemetry_cache
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    /// Appends an alert to the in-memory reseed cache and prunes old entries.
    pub fn cache_recent_alert(&self, alert: AlertDto) {
        const CACHE_WINDOW_MS: i64 = 20 * 60 * 1000;
        const CACHE_MAX_ROWS: usize = 4_096;

        let mut q = self.recent_alerts_cache.lock().unwrap();
        q.push_back(alert);

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

    /// Returns a point-in-time clone of the recent alert cache.
    pub fn recent_alerts_snapshot(&self) -> Vec<AlertDto> {
        self.recent_alerts_cache
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }
}

fn modeled_board_endpoints(
    board: Board,
    simulated: bool,
    local_endpoint_list: &[String],
    rocket_side_endpoints: &[String],
    fill_side_endpoints: &[String],
) -> Vec<String> {
    if simulated {
        return match board {
            Board::GroundStation => local_endpoint_list.to_vec(),
            Board::RFBoard => {
                let mut endpoints = crate::flight_sim::simulated_board_endpoints(board);
                endpoints.extend_from_slice(rocket_side_endpoints);
                endpoints.sort();
                endpoints.dedup();
                endpoints
            }
            Board::GatewayBoard => {
                let mut endpoints = crate::flight_sim::simulated_board_endpoints(board);
                endpoints.extend_from_slice(fill_side_endpoints);
                endpoints.sort();
                endpoints.dedup();
                endpoints
            }
            _ => crate::flight_sim::simulated_board_endpoints(board),
        };
    }

    let mut endpoints = match board {
        Board::GroundStation => local_endpoint_list.to_vec(),
        Board::FlightComputer => vec![
            DataEndpoint::FlightController.as_str().to_string(),
            DataEndpoint::FlightState.as_str().to_string(),
            DataEndpoint::SdCard.as_str().to_string(),
        ],
        Board::RFBoard => rocket_side_endpoints.to_vec(),
        Board::PowerBoard => Vec::new(),
        Board::ValveBoard => vec![
            DataEndpoint::ValveBoard.as_str().to_string(),
            DataEndpoint::Abort.as_str().to_string(),
        ],
        Board::GatewayBoard => fill_side_endpoints.to_vec(),
        Board::ActuatorBoard => vec![
            DataEndpoint::ActuatorBoard.as_str().to_string(),
            DataEndpoint::Abort.as_str().to_string(),
        ],
        Board::DaqBoard => Vec::new(),
    };

    endpoints.sort();
    endpoints.dedup();
    endpoints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flight_computer_modeled_endpoints_include_sd_card() {
        let endpoints = modeled_board_endpoints(Board::FlightComputer, false, &[], &[], &[]);
        assert!(
            endpoints
                .iter()
                .any(|endpoint| endpoint == DataEndpoint::SdCard.as_str())
        );
    }
}
