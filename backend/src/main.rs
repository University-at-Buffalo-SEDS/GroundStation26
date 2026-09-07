// main.rs

macro_rules! gs_debug_println {
    ($($arg:tt)*) => {
        if crate::debug_prints_enabled() {
            std::println!($($arg)*);
        }
    };
}

mod auth;
mod comms;
mod comms_config;
#[cfg(feature = "testing")]
mod dummy_packets;
mod fill_targets;
mod firmware_update;
mod flight_setup;
mod flight_sim;
mod gpio;
mod gpio_panel;
mod i18n;
mod layout;
mod loadcell;
mod logger;
mod map;
mod network_variables;
mod ring_buffer;
mod rocket_commands;
#[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
mod safety_task;
mod sequences;
mod state;
mod telemetry_db;
mod telemetry_schema;
mod telemetry_task;
#[cfg(feature = "test_fire_mode")]
mod test_fire_csv;
mod types;
mod web;

use crate::map::{DEFAULT_MAP_REGION, ensure_map_data};
use crate::ring_buffer::RingBuffer;
use crate::rocket_commands::ValveBoardCommands;
#[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
use crate::safety_task::safety_task;
use crate::sequences::{default_action_policy, start_sequence_task};
use crate::state::{AppState, BoardStatus};
use crate::telemetry_db::{
    DEFAULT_TELEMETRY_DB_FILENAME, DbQueueItem, LaunchClockMsg, RecordingModeWire,
    RecordingStatusMsg, apply_sqlite_pragmas, close_and_finalize_sqlite, delete_sqlite_if_empty,
    ensure_sqlite_db_file, open_in_memory_telemetry_db, recover_sqlite_sidecars_in_dir,
};
use crate::telemetry_task::{
    CommsWorkerHandle, flush_command_tx, get_current_timestamp_ms, set_network_time_router,
    telemetry_task,
};

#[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
use crate::comms::DummyComms;
use crate::comms::{CommsDevice, link_description, open_link, startup_failure_hint};
use crate::comms_config::CommsLinkConfig;
use crate::types::{Board, FlightState as FlightStateMode};
use axum::Router;
use sedsnet::TelemetryError;
use sedsnet::packet::Packet;
use sedsnet::router::{EndpointHandler, RouterSideOptions};
use sedsnet::timesync::{TimeSyncConfig, TimeSyncRole};
use sqlx::Row;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::{Duration, Instant};

use crate::web::AlertAckStateMsg;
use crate::web::emit_error;
use tokio::sync::{broadcast, mpsc};

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn network_router_time_divisor() -> u64 {
    std::env::var("GS_SIM_ROUTER_TIME_DIVISOR")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(1)
}

fn validation_elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) / network_router_time_divisor()
}

fn validation_latency_limit_ms(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

async fn wait_for_validation_reliable_delivery(
    router: &sedsnet::router::Router,
    label: &str,
    tx_before: &[(sedsnet::config::DataType, u64)],
) -> bool {
    let started = Instant::now();
    let latency_limit_ms =
        validation_latency_limit_ms("GS_SIM_MANAGED_VARIABLE_MAX_LATENCY_MS", 2_500);
    let timeout_ms = std::env::var("GS_SIM_RELIABLE_ACK_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(30_000);
    let minimum_observation_ms = std::env::var("GS_SIM_MANAGED_VARIABLE_SETTLE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(500);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let stats = router.export_runtime_stats();
        let pending = stats.reliable.end_to_end_pending_destination_count;
        let transmission_counts = tx_before
            .iter()
            .map(|(expected, before)| {
                let now = stats
                    .sides
                    .iter()
                    .flat_map(|side| side.data_types.iter())
                    .filter(|kind| kind.data_type == *expected)
                    .map(|kind| kind.tx_packets)
                    .sum::<u64>();
                (*expected, *before, now)
            })
            .collect::<Vec<_>>();
        let every_update_transmitted = transmission_counts
            .iter()
            .all(|(_, before, now)| now > before);
        let elapsed_ms = validation_elapsed_ms(started);
        if elapsed_ms >= minimum_observation_ms
            && every_update_transmitted
            && stats.queues.tx_len == 0
            && pending == 0
        {
            if elapsed_ms > latency_limit_ms {
                log::error!(
                    "full-bay reliable delivery exceeded latency bound for {label}: {elapsed_ms} ms > {latency_limit_ms} ms"
                );
                return false;
            }
            log::info!(
                "full-bay managed-variable latency within bound: {elapsed_ms} ms <= {latency_limit_ms} ms ({label}); per-type tx before/after={transmission_counts:?}"
            );
            return true;
        }
        if Instant::now() >= deadline {
            log::error!(
                "full-bay reliable delivery timed out for {label}: every update transmitted={every_update_transmitted}, {pending} destination ACK(s) pending, tx queue depth {}, per-type tx before/after={transmission_counts:?}",
                stats.queues.tx_len,
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) fn debug_prints_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_DEBUG_PRINTS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

pub(crate) fn radio_diagnostics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_RADIO_DIAGNOSTICS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

pub(crate) fn ws_diagnostics_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GS_WS_DIAGNOSTICS")
            .ok()
            .map(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

fn router_hop_reliable_enabled(link: &CommsLinkConfig) -> bool {
    match link {
        // Pico-Fi's polled I2C bridge owns delivery and cannot sustain
        // SEDSNet's bidirectional hop-ACK traffic without starving Gateway.
        CommsLinkConfig::I2c { .. } => false,
        // Raw UART only provides outer framing on the RFD900x hop, so SEDSNet
        // retains delivery semantics there.
        CommsLinkConfig::Serial { .. }
        | CommsLinkConfig::RaspberryPiGpioUart { .. }
        | CommsLinkConfig::CustomSerial { .. } => true,
        CommsLinkConfig::Spi { .. } | CommsLinkConfig::Can { .. } => true,
    }
}

#[cfg(test)]
mod router_link_policy_tests {
    use super::*;

    #[test]
    fn raw_uart_radio_keeps_sedsnet_reliability_enabled() {
        let link = CommsLinkConfig::Serial {
            serial: crate::comms_config::SerialLinkConfig {
                port: "sim://av-bay".to_owned(),
                baud_rate: 57_600,
                protocol: crate::comms_config::SerialProtocol::RawUart,
            },
        };

        assert!(router_hop_reliable_enabled(&link));
    }

    #[test]
    fn pico_fi_i2c_uses_transport_delivery_without_hop_retries() {
        let link = CommsLinkConfig::I2c {
            i2c: crate::comms_config::I2cLinkConfig {
                bus: 1,
                addr: 0x17,
                chunk_delay_ms: 1,
                initial_wait_ms: 0,
            },
        };

        assert!(!router_hop_reliable_enabled(&link));
    }
}

/// Creates or upgrades the auth session table used by token-based login.
async fn ensure_auth_sessions_table(db: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS auth_sessions (
            token             TEXT PRIMARY KEY,
            username          TEXT NOT NULL,
            session_type      TEXT NOT NULL,
            can_view_data     INTEGER NOT NULL,
            can_send_commands INTEGER NOT NULL,
            allowed_commands_json TEXT NOT NULL DEFAULT '[]',
            created_at_ms     INTEGER NOT NULL,
            expires_at_ms     INTEGER NOT NULL
        );
        "#,
    )
    .execute(db)
    .await?;

    let session_columns = sqlx::query("PRAGMA table_info(auth_sessions);")
        .fetch_all(db)
        .await?;
    let has_allowed_commands_json = session_columns.iter().any(|row| {
        row.get::<String, _>("name")
            .eq_ignore_ascii_case("allowed_commands_json")
    });
    if !has_allowed_commands_json {
        sqlx::query(
            "ALTER TABLE auth_sessions ADD COLUMN allowed_commands_json TEXT NOT NULL DEFAULT '[]';",
        )
            .execute(db)
            .await?;
    }

    Ok(())
}

/// Waits for process termination signals and then fan-outs the app-wide shutdown request.
async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log::error!("failed to install Ctrl+C handler: {err}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                log::error!("failed to install SIGTERM handler: {err}");
            }
        }
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    #[cfg(not(unix))]
    ctrl_c.await;

    if state.request_shutdown() {
        log::info!("shutdown requested from signal handler");
    }
}

fn open_rocket_comms(link: &CommsLinkConfig) -> (Arc<Mutex<Box<dyn CommsDevice>>>, bool) {
    match open_link(link) {
        Ok(r) => {
            gs_debug_println!("Rocket comms online");
            (Arc::new(Mutex::new(r)), true)
        }
        Err(e) => {
            gs_debug_println!("Rocket comms missing, using DummyComms: {}", e);
            log::warn!(
                "AV bay link unavailable: {e}. Setup hint: {}",
                startup_failure_hint(link)
            );
            #[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
            {
                (
                    Arc::new(Mutex::new(Box::new(DummyComms::new("Rocket Comms")))),
                    false,
                )
            }
            #[cfg(not(feature = "testing"))]
            #[cfg(not(feature = "hitl_mode"))]
            #[cfg(not(feature = "test_fire_mode"))]
            panic!("Rocket comms missing and testing mode not enabled")
        }
    }
}

fn open_umbilical_comms(link: &CommsLinkConfig) -> (Arc<Mutex<Box<dyn CommsDevice>>>, bool) {
    match open_link(link) {
        Ok(r) => {
            gs_debug_println!("Umbilical comms online");
            (Arc::new(Mutex::new(r)), true)
        }
        Err(e) => {
            gs_debug_println!("Umbilical comms missing, using DummyComms: {}", e);
            log::warn!(
                "Fill box link unavailable: {e}. Setup hint: {}",
                startup_failure_hint(link)
            );
            #[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
            {
                (
                    Arc::new(Mutex::new(Box::new(DummyComms::new("Umbilical Comms")))),
                    false,
                )
            }
            #[cfg(not(feature = "testing"))]
            #[cfg(not(feature = "hitl_mode"))]
            #[cfg(not(feature = "test_fire_mode"))]
            panic!("Umbilical comms missing and testing mode not enabled")
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry_schema::initialize()?;

    logger::init()?;
    log::info!(
        "groundstation backend starting features testing={} hitl_mode={} test_fire_mode={}",
        cfg!(feature = "testing"),
        cfg!(feature = "hitl_mode"),
        cfg!(feature = "test_fire_mode")
    );

    // Initialize GPIO
    let gpio = gpio::GpioPins::new();

    // Ensure offline map tiles
    if let Err(e) = ensure_map_data(DEFAULT_MAP_REGION).await {
        log::warn!("failed to ensure map tiles: {e:#}");
        // you can choose to return Err(e) instead if tiles are mandatory
    }

    // --- DB path ---
    let recordings_dir = PathBuf::from("./data");
    let placeholder_db_path = recordings_dir.join(DEFAULT_TELEMETRY_DB_FILENAME);
    recover_sqlite_sidecars_in_dir(&recordings_dir).await?;
    if placeholder_db_path.exists() {
        let _ = delete_sqlite_if_empty(&placeholder_db_path.to_string_lossy()).await;
    }
    let db = open_in_memory_telemetry_db().await?;
    let auth_db_path = recordings_dir.join("users.db");
    let auth_db_path_str = ensure_sqlite_db_file(&auth_db_path)?;
    let auth_db = sqlx::SqlitePool::connect(&format!("sqlite://{}", auth_db_path_str)).await?;
    apply_sqlite_pragmas(&auth_db).await;

    ensure_auth_sessions_table(&auth_db).await?;

    // --- Channels ---
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let db_work_queue_size = env_usize("GS_DB_WORK_QUEUE_SIZE", 8_192, 1024, 262_144);
    let (db_queue_tx, db_queue_rx) = mpsc::channel::<DbQueueItem>(db_work_queue_size);
    let ws_broadcast_capacity = env_usize("GS_WS_BROADCAST_CAPACITY", 8192, 512, 262_144);
    let board_status_capacity = env_usize("GS_BOARD_STATUS_BROADCAST_CAPACITY", 256, 64, 4096);
    let alerts_capacity = env_usize("GS_ALERTS_BROADCAST_CAPACITY", 1024, 128, 8192);
    let notifications_capacity = env_usize("GS_NOTIFICATIONS_BROADCAST_CAPACITY", 64, 16, 2048);
    let actions_capacity = env_usize("GS_ACTION_POLICY_BROADCAST_CAPACITY", 64, 16, 2048);
    let launch_clock_capacity = env_usize("GS_LAUNCH_CLOCK_BROADCAST_CAPACITY", 32, 8, 1024);
    let recording_status_capacity =
        env_usize("GS_RECORDING_STATUS_BROADCAST_CAPACITY", 32, 8, 1024);
    let (ws_tx, _ws_rx) = broadcast::channel(ws_broadcast_capacity);
    let (board_status_tx, _board_status_rx) = broadcast::channel(board_status_capacity);
    let (dashboard_reset_tx, _dashboard_reset_rx) = broadcast::channel(16);
    let (notifications_tx, _notifications_rx) = broadcast::channel(notifications_capacity);
    let (messages_tx, _messages_rx) = broadcast::channel(notifications_capacity);
    let (action_policy_tx, _action_policy_rx) = broadcast::channel(actions_capacity);
    let (fill_targets_tx, _fill_targets_rx) = broadcast::channel(actions_capacity);
    let (launch_clock_tx, _launch_clock_rx) = broadcast::channel(launch_clock_capacity);
    let (recording_status_tx, _recording_status_rx) = broadcast::channel(recording_status_capacity);
    let (shutdown_tx, _shutdown_rx) = broadcast::channel(8);

    // --- Shared state ---
    let mut board_status = HashMap::new();
    for board in Board::ALL {
        board_status.insert(
            *board,
            BoardStatus {
                packet_count: 0,
                last_seen_ms: None,
                last_seen_instant: None,
                ema_gap_ms: None,
                warned: false,
            },
        );
    }

    let ring_buffer_capacity = env_usize("GS_RING_BUFFER_CAPACITY", 65_536, 1024, 1_000_000);
    let loadcell_calibration = loadcell::load_or_default();
    let comms_links = comms_config::load_or_default();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let users_path = manifest_dir.join("users").join("users.json");
    let legacy_users_path = manifest_dir.join("data").join("users.json");
    if !users_path.exists() && legacy_users_path.exists() {
        if let Some(parent) = users_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&legacy_users_path, &users_path)?;
    }
    let auth = Arc::new(auth::AuthManager::new(users_path));
    auth.ensure_file()
        .map_err(|e| anyhow::anyhow!("failed to initialize users.json: {e}"))?;
    let _ = auth.cleanup_expired_sessions(&auth_db).await;
    let state = Arc::new(AppState {
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(ring_buffer_capacity))),
        cmd_tx,
        ws_tx,
        warnings_tx: broadcast::channel(alerts_capacity).0,
        errors_tx: broadcast::channel(alerts_capacity).0,
        alert_ack_state: Arc::new(Mutex::new(AlertAckStateMsg::default())),
        alert_ack_tx: broadcast::channel(16).0,
        dashboard_reset_tx,
        db: Arc::new(Mutex::new(db)),
        db_path: Arc::new(Mutex::new("sqlite::memory:".to_string())),
        placeholder_db_path: placeholder_db_path.to_string_lossy().to_string(),
        db_queue_tx,
        auth_db,
        state: Arc::new(Mutex::new(
            crate::types::u8_to_flight_state(network_variables::flight_state())
                .unwrap_or(FlightStateMode::Startup),
        )),
        state_tx: broadcast::channel(16).0,
        gpio,
        board_status: Arc::new(Mutex::new(board_status)),
        board_status_tx,
        last_board_status_broadcast_ms: Arc::new(AtomicU64::new(0)),
        last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
        umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        pending_umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
        latest_fill_mass_kg: Arc::new(Mutex::new(None)),
        loadcell_calibration: Arc::new(Mutex::new(loadcell_calibration)),
        shutdown_tx,
        shutdown_requested: Arc::new(AtomicBool::new(false)),
        pending_db_writes: Arc::new(AtomicUsize::new(0)),
        db_write_notify: Arc::new(Notify::new()),
        notifications: Arc::new(Mutex::new(Vec::new())),
        notifications_tx,
        next_notification_id: Arc::new(AtomicU64::new(0)),
        messages: Arc::new(Mutex::new(Vec::new())),
        messages_tx,
        next_message_id: Arc::new(AtomicU64::new(0)),
        action_policy: Arc::new(Mutex::new(default_action_policy())),
        sequence_policy_state: Arc::new(Mutex::new(
            crate::sequences::SequencePolicyState::default(),
        )),
        action_policy_tx,
        fill_targets: Arc::new(Mutex::new(fill_targets::load_or_default())),
        fill_targets_tx,
        launch_clock: Arc::new(Mutex::new(LaunchClockMsg::idle())),
        launch_clock_tx,
        launch_sequence_command_pending: Arc::new(AtomicBool::new(false)),
        launch_indicator_latched: Arc::new(AtomicBool::new(false)),
        abort_indicator_latched: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "hitl_mode")]
        hitl_button_interlock_enabled: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "hitl_mode")]
        hitl_launch_interlock_enabled: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "hitl_mode")]
        hitl_physical_launch_uses_ground_station: Arc::new(AtomicBool::new(false)),
        recording_status: Arc::new(Mutex::new(RecordingStatusMsg {
            mode: RecordingModeWire::Idle,
            db_path: None,
        })),
        recording_status_tx,
        last_command_ms: Arc::new(Mutex::new(HashMap::new())),
        fill_sequence_continue_requests: Arc::new(AtomicU64::new(0)),
        recent_telemetry_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        latest_gps_fix_by_sender: Arc::new(Mutex::new(HashMap::new())),
        latest_gps_satellites_by_sender: Arc::new(Mutex::new(HashMap::new())),
        recent_alerts_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        av_bay_comms_connected: Arc::new(AtomicBool::new(false)),
        fill_comms_connected: Arc::new(AtomicBool::new(false)),
        topology_router: Arc::new(std::sync::OnceLock::new()),
        auth,
    });

    state.set_messages_snapshot(Vec::new());

    gpio_panel::setup_gpio_panel(state.clone()).expect("failed to setup gpio panel");
    let sequence_shutdown_rx = state.shutdown_subscribe();
    let mut sequence_task = start_sequence_task(state.clone(), sequence_shutdown_rx);

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let pilot_open_ack_generation = Arc::new(AtomicU64::new(0));
    let pilot_open_ack_generation_handler = pilot_open_ack_generation.clone();
    let abort_handler_state_clone = state.clone();
    let flight_state_handler_state_clone = state.clone();
    let heartbeat_handler_state_clone = state.clone();

    let ground_station_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("GROUND_STATION"),
        move |pkt: &Packet| {
            if pkt.data_type() == telemetry_schema::data_type("UMBILICAL_STATUS") {
                log::info!(
                    "umbilical status packet received sender={} endpoints={:?} payload={:02x?}",
                    pkt.sender(),
                    pkt.endpoints(),
                    pkt.payload()
                );
                if pkt.payload() == [ValveBoardCommands::PilotOpen as u8, 1].as_slice() {
                    pilot_open_ack_generation_handler.fetch_add(1, Ordering::Relaxed);
                }
            }
            ground_station_handler_state_clone
                .mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            ground_station_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = ground_station_handler_state_clone
                .ring_buffer
                .lock()
                .unwrap();
            rb.push(pkt.clone());
            Ok(())
        },
    );

    let flight_state_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("FLIGHT_STATE"),
        move |pkt: &Packet| {
            flight_state_handler_state_clone
                .mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            flight_state_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = flight_state_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(pkt.clone());
            Ok(())
        },
    );

    let abort_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("ABORT"),
        move |pkt: &Packet| {
            abort_handler_state_clone.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            abort_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            abort_handler_state_clone.clear_launch_sequence_command_pending();
            abort_handler_state_clone.set_abort_indicator_latched(true);
            crate::sequences::refresh_action_policy_now(&abort_handler_state_clone);
            abort_handler_state_clone.broadcast_action_policy_snapshot();
            let error_msg = pkt
                .data_as_string()
                .unwrap_or_else(|_| String::from_utf8_lossy(pkt.payload()).into_owned());
            log::error!(
                "abort packet received sender={} endpoints={:?} message={error_msg}",
                pkt.sender(),
                pkt.endpoints()
            );
            emit_error(&abort_handler_state_clone, error_msg);
            Ok(())
        },
    );

    let heartbeat_handler = EndpointHandler::new_packet_handler(
        telemetry_schema::endpoint("HEART_BEAT"),
        move |pkt: &Packet| {
            heartbeat_handler_state_clone.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            heartbeat_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = heartbeat_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(pkt.clone());
            Ok(())
        },
    );

    let mut cfg = sedsnet::router::RouterConfig::new([
        ground_station_handler,
        abort_handler,
        flight_state_handler,
        heartbeat_handler,
    ]);
    if telemetry_task::timesync_enabled() {
        cfg = cfg.with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Source,
            priority: 50,
            ..TimeSyncConfig::default()
        });
    }

    // --- Radios ---
    gs_debug_println!("AV bay config: {}", link_description(&comms_links.av_bay));
    gs_debug_println!(
        "Fill box config: {}",
        link_description(&comms_links.fill_box)
    );

    #[cfg(feature = "testing")]
    let force_sim_comms = flight_sim::sim_mode_enabled();

    #[cfg(feature = "testing")]
    let (rocket_comms, av_bay_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        if force_sim_comms {
            gs_debug_println!("Testing simulator mode enabled; using DummyComms for rocket comms");
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Rocket Comms")))),
                false,
            )
        } else {
            open_rocket_comms(&comms_links.av_bay)
        };
    #[cfg(not(feature = "testing"))]
    let (rocket_comms, av_bay_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        open_rocket_comms(&comms_links.av_bay);

    #[cfg(feature = "testing")]
    let (umbilical_comms, fill_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        if force_sim_comms {
            gs_debug_println!(
                "Testing simulator mode enabled; using DummyComms for umbilical comms"
            );
            (
                Arc::new(Mutex::new(Box::new(DummyComms::new("Umbilical Comms")))),
                false,
            )
        } else {
            open_umbilical_comms(&comms_links.fill_box)
        };
    #[cfg(not(feature = "testing"))]
    let (umbilical_comms, fill_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        open_umbilical_comms(&comms_links.fill_box);
    state
        .av_bay_comms_connected
        .store(av_bay_comms_connected, Ordering::Relaxed);
    state
        .fill_comms_connected
        .store(fill_comms_connected, Ordering::Relaxed);

    let router = Arc::new(if network_router_time_divisor() == 1 {
        sedsnet::router::Router::new(cfg)
    } else {
        // Renode advances several MCU instances cooperatively and can run much
        // slower than wall time. Keep discovery expiry on the simulated time
        // scale so valid firmware routes do not disappear only because the
        // host process runs natively. Production defaults to a divisor of one.
        let divisor = network_router_time_divisor();
        let started = std::time::Instant::now();
        sedsnet::router::Router::new_with_clock(
            cfg,
            Box::new(move || {
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) / divisor
            }),
        )
    });
    network_variables::initialize(&router)?;
    set_network_time_router(router.clone());
    let _ = state.topology_router.set(router.clone());

    let (rocket_tx, rocket_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let (umbilical_tx, umbilical_rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();

    let rocket_side = {
        let rocket_tx = rocket_tx.clone();
        let opts = RouterSideOptions {
            reliable_enabled: router_hop_reliable_enabled(&comms_links.av_bay),
            ..Default::default()
        }
        .with_small_packet_transport(1024);
        router.add_side_packed_with_priority_and_options(
            "rocket_comms",
            move |pkt, priority| {
                rocket_tx
                    .send((priority, pkt.to_vec()))
                    .map_err(|_| TelemetryError::HandlerError("rocket_comms tx queue closed"))?;
                Ok(())
            },
            opts,
        )
    };

    let umbilical_side = {
        let umbilical_tx = umbilical_tx.clone();
        let opts = RouterSideOptions {
            reliable_enabled: router_hop_reliable_enabled(&comms_links.fill_box),
            ..Default::default()
        }
        .with_small_packet_transport(1024);
        router.add_side_packed_with_priority_and_options(
            "umbilical_comms",
            move |pkt, priority| {
                umbilical_tx
                    .send((priority, pkt.to_vec()))
                    .map_err(|_| TelemetryError::HandlerError("umbilical_comms tx queue closed"))?;
                Ok(())
            },
            opts,
        )
    };

    rocket_comms
        .lock()
        .expect("failed to get rocket comms lock")
        .set_side_id(rocket_side);
    umbilical_comms
        .lock()
        .expect("failed to get umbilical comms lock")
        .set_side_id(umbilical_side);

    network_variables::publish_current(&router)?;
    // The full-system simulator uses the production toggle path while each
    // phase starts a fresh GroundStation process. Keeping this behind an
    // explicit validation-only environment flag avoids altering deployment
    // startup behavior.
    if std::env::var("GS_SIM_TOGGLE_UNDERGLOW_ON_START")
        .ok()
        .as_deref()
        == Some("1")
    {
        let enabled = network_variables::toggle_underglow(&router)?;
        log::info!("full-bay validation toggled AV bay underglow: {enabled}");
    }

    let initial_discovery = if std::env::var("GS_SIM_COMPACT_INITIAL_DISCOVERY")
        .ok()
        .as_deref()
        == Some("1")
    {
        /* Every linked-validation firmware image embeds this same schema.
         * Send the normal lightweight topology/address refresh and omit the
         * redundant multi-kilobyte hosted schema transfer. */
        router.poll_discovery().map(|_| ())
    } else {
        router.announce_discovery()
    };
    if let Err(err) = initial_discovery {
        eprintln!("WARNING: failed to queue initial discovery announce: {err}");
    }

    // --- Background tasks ---
    let telemetry_shutdown_rx = state.shutdown_subscribe();
    let safety_shutdown_rx = state.shutdown_subscribe();
    let mut tt = tokio::spawn(telemetry_task(
        state.clone(),
        router.clone(),
        vec![
            CommsWorkerHandle {
                name: "rocket_comms",
                comms: rocket_comms,
                tx_comms: None,
                side_id: rocket_side,
                tx_rx: rocket_rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
            CommsWorkerHandle {
                name: "umbilical_comms",
                comms: umbilical_comms,
                tx_comms: None,
                side_id: umbilical_side,
                tx_rx: umbilical_rx,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        ],
        cmd_rx,
        db_queue_rx,
        telemetry_shutdown_rx,
    ));
    let full_bay_discovery_ready = Arc::new(AtomicBool::new(false));
    if let Ok(expected) = std::env::var("GS_SIM_EXPECT_DISCOVERY_NODES") {
        let validation_router = router.clone();
        let validation_discovery_ready = full_bay_discovery_ready.clone();
        let validation_pilot_ack_generation = pilot_open_ack_generation.clone();
        let validate_valve_roundtrip = std::env::var("GS_SIM_VALIDATE_VALVE_ROUNDTRIP")
            .ok()
            .as_deref()
            == Some("1");
        let control_step_ms = std::env::var("GS_SIM_CONTROL_STEP_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(750);
        tokio::spawn(async move {
            let expected = expected
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            loop {
                let topology = validation_router.export_topology();
                let mut discovered = BTreeSet::new();
                for route in topology.routes {
                    for announcer in route.announcers {
                        discovered.insert(announcer.sender_id);
                        for board in announcer.routers {
                            discovered.insert(board.sender_id);
                        }
                    }
                }
                if expected.is_subset(&discovered) {
                    validation_discovery_ready.store(true, Ordering::Release);
                    log::info!(
                        "full-bay named discovery ready: {}",
                        discovered.into_iter().collect::<Vec<_>>().join(",")
                    );
                    if validate_valve_roundtrip {
                        let deadline = Instant::now() + Duration::from_secs(30);
                        while validation_pilot_ack_generation.load(Ordering::Acquire) == 0 {
                            if Instant::now() >= deadline {
                                log::error!(
                                    "full-bay managed-variable validation timed out waiting for Valve command acknowledgement"
                                );
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    let flight_states = std::env::var("GS_SIM_FLIGHT_STATE_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .filter_map(|value| match value.trim().parse::<u8>() {
                                    Ok(state) => Some(state),
                                    Err(_) => {
                                        log::error!(
                                            "full-bay flight-state sequence contains invalid value {value:?}"
                                        );
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let underglow = std::env::var("GS_SIM_UNDERGLOW_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .map(|value| matches!(value.trim(), "1" | "true" | "on"))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let flight_buzzer = std::env::var("GS_SIM_FLIGHT_BUZZER_SEQUENCE")
                        .ok()
                        .map(|sequence| {
                            sequence
                                .split(',')
                                .map(|value| matches!(value.trim(), "1" | "true" | "on"))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let rounds = flight_states
                        .len()
                        .max(underglow.len())
                        .max(flight_buzzer.len());
                    for round in 0..rounds {
                        tokio::time::sleep(Duration::from_millis(control_step_ms)).await;
                        let expected_types = [
                            flight_states
                                .get(round)
                                .map(|_| telemetry_schema::data_type("FLIGHT_STATE")),
                            underglow
                                .get(round)
                                .map(|_| telemetry_schema::data_type("AV_BAY_UNDERGLOW")),
                            flight_buzzer
                                .get(round)
                                .map(|_| telemetry_schema::data_type("FLIGHT_BUZZER")),
                        ]
                        .into_iter()
                        .flatten()
                        .map(|data_type| {
                            let count = validation_router
                                .export_runtime_stats()
                                .sides
                                .iter()
                                .flat_map(|side| side.data_types.iter())
                                .filter(|kind| kind.data_type == data_type)
                                .map(|kind| kind.tx_packets)
                                .sum::<u64>();
                            (data_type, count)
                        })
                        .collect::<Vec<_>>();
                        if let Some(&state) = flight_states.get(round) {
                            if let Err(error) =
                                network_variables::set_flight_state(&validation_router, state)
                            {
                                log::error!("full-bay flight-state sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set flight state: {state}");
                        }
                        if let Some(&enabled) = underglow.get(round) {
                            if let Err(error) =
                                network_variables::set_underglow(&validation_router, enabled)
                            {
                                log::error!("full-bay underglow sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set AV bay underglow: {enabled}");
                        }
                        if let Some(&enabled) = flight_buzzer.get(round) {
                            if let Err(error) =
                                network_variables::set_flight_buzzer(&validation_router, enabled)
                            {
                                log::error!("full-bay Flight buzzer sequence failed: {error}");
                                break;
                            }
                            log::info!("full-bay validation set Flight buzzer: {enabled}");
                        }
                        flush_command_tx(
                            &validation_router,
                            "full-bay managed-variable validation tx",
                        );
                        if !wait_for_validation_reliable_delivery(
                            &validation_router,
                            "managed-variable round",
                            &expected_types,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    if std::env::var_os("GS_SIM_EXPECT_DISCOVERY_NODES").is_none()
        && let Ok(sequence) = std::env::var("GS_SIM_UNDERGLOW_SEQUENCE")
    {
        let validation_router = router.clone();
        tokio::spawn(async move {
            for value in sequence.split(',') {
                tokio::time::sleep(Duration::from_millis(750)).await;
                let enabled = matches!(value.trim(), "1" | "true" | "on");
                match network_variables::set_underglow(&validation_router, enabled) {
                    Ok(()) => log::info!("full-bay validation set AV bay underglow: {enabled}"),
                    Err(error) => {
                        log::error!("full-bay underglow sequence failed: {error}");
                        break;
                    }
                }
            }
        });
    }
    if std::env::var_os("GS_SIM_EXPECT_DISCOVERY_NODES").is_none()
        && let Ok(sequence) = std::env::var("GS_SIM_FLIGHT_BUZZER_SEQUENCE")
    {
        let validation_router = router.clone();
        tokio::spawn(async move {
            for value in sequence.split(',') {
                tokio::time::sleep(Duration::from_millis(750)).await;
                let enabled = matches!(value.trim(), "1" | "true" | "on");
                match network_variables::set_flight_buzzer(&validation_router, enabled) {
                    Ok(()) => {
                        log::info!("full-bay validation set Flight buzzer: {enabled}")
                    }
                    Err(error) => {
                        log::error!("full-bay Flight buzzer sequence failed: {error}");
                        break;
                    }
                }
            }
        });
    }
    if std::env::var("GS_SIM_VALIDATE_VALVE_ROUNDTRIP")
        .ok()
        .as_deref()
        == Some("1")
    {
        let validation_router = router.clone();
        let validation_state = state.clone();
        let validation_pilot_ack_generation = pilot_open_ack_generation.clone();
        let validation_discovery_ready = full_bay_discovery_ready.clone();
        tokio::spawn(async move {
            let discovery_started = Instant::now();
            let valve_endpoint = telemetry_schema::endpoint("VALVE_BOARD");
            // Command validation must exercise learned discovery routing. The
            // linked simulator advances virtual MCU time more slowly than host
            // wall time, so a fixed host-side timeout can fire before the
            // Valve discovery announcement crosses CAN, Gateway, Pico-Fi, and
            // I2C. Wait for the actual route instead of injecting a fanout.
            let mut waits = 0u32;
            loop {
                if validation_discovery_ready.load(Ordering::Acquire)
                    && validation_router
                        .export_topology()
                        .routes
                        .iter()
                        .any(|route| {
                            route.side_name == "umbilical_comms"
                                && route.reachable_endpoints.contains(&valve_endpoint)
                        })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                waits += 1;
                if waits.is_multiple_of(100) {
                    log::info!("full-bay validation waiting for Valve discovery route");
                }
            }
            let discovery_elapsed_ms = validation_elapsed_ms(discovery_started);
            let discovery_limit_ms =
                validation_latency_limit_ms("GS_SIM_DISCOVERY_MAX_LATENCY_MS", 5_000);
            if discovery_elapsed_ms > discovery_limit_ms {
                log::error!(
                    "full-bay discovery exceeded latency bound: {discovery_elapsed_ms} ms > {discovery_limit_ms} ms"
                );
                return;
            }
            log::info!(
                "full-bay discovery latency within bound: {discovery_elapsed_ms} ms <= {discovery_limit_ms} ms"
            );
            log::info!("full-bay valve discovery route is ready");
            log::info!(
                "full-bay Valve discovery topology: {:?}",
                validation_router.export_topology().routes
            );

            // Incremental discovery propagates additions in both directions.
            // Do not force a full topology refresh before each command: that
            // would hide discovery regressions and waste bandwidth.
            let settle_ms = std::env::var("GS_SIM_VALVE_ROUTE_SETTLE_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(500);
            tokio::time::sleep(Duration::from_millis(settle_ms)).await;

            let command_type = telemetry_schema::data_type("VALVE_COMMAND");
            /* Exercise the same runtime-schema path as the UI. log_queue uses
             * the VALVE_COMMAND endpoint ownership learned by discovery;
             * manually constructing a packet here bypassed that schema lookup
             * and could leave it local to the GroundStation. */
            let ack_generation_before = validation_pilot_ack_generation.load(Ordering::Relaxed);
            let command_started = Instant::now();
            let ack_latency_limit_ms =
                validation_latency_limit_ms("GS_SIM_VALVE_ACK_MAX_LATENCY_MS", 2_500);
            let open_result =
                validation_router.log_queue(command_type, &[ValveBoardCommands::PilotOpen as u8]);
            if let Err(err) = open_result {
                log::error!("full-bay valve-open validation command failed: {err}");
                return;
            }
            flush_command_tx(&validation_router, "full-bay valve-open validation tx");
            log::info!(
                "full-bay router stats after valve command: {:?}",
                validation_router.export_runtime_stats().sides
            );
            log::info!("full-bay valve-open validation command queued");

            // The linked firmware simulator advances seven MCUs cooperatively;
            // a packet that is near-instantaneous on hardware can take tens of
            // seconds of host time to traverse both emulated serial bridges.
            loop {
                if validation_pilot_ack_generation.load(Ordering::Relaxed) > ack_generation_before
                    && validation_state
                        .get_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8)
                        == Some(true)
                {
                    let elapsed_ms = validation_elapsed_ms(command_started);
                    if elapsed_ms > ack_latency_limit_ms {
                        log::error!(
                            "full-bay valve ACK exceeded latency bound: {elapsed_ms} ms > {ack_latency_limit_ms} ms"
                        );
                        return;
                    }
                    log::info!(
                        "full-bay valve ACK latency within bound: {elapsed_ms} ms <= {ack_latency_limit_ms} ms"
                    );
                    log::info!("full-bay valve ACK reached GroundStation");
                    return;
                }
                if validation_elapsed_ms(command_started) > ack_latency_limit_ms {
                    log::error!(
                        "full-bay valve ACK timed out: generation before={}, generation now={}, app state={:?}, topology={:?}, router stats={:?}",
                        ack_generation_before,
                        validation_pilot_ack_generation.load(Ordering::Relaxed),
                        validation_state
                            .get_umbilical_valve_state(ValveBoardCommands::PilotOpen as u8),
                        validation_router.export_topology().routes,
                        validation_router.export_runtime_stats().sides,
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    #[cfg(not(any(feature = "hitl_mode", feature = "test_fire_mode")))]
    let mut st = tokio::spawn(safety_task(
        state.clone(),
        router.clone(),
        safety_shutdown_rx,
    ));
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    let mut st = tokio::spawn(async move {
        let _ = safety_shutdown_rx;
    });

    // --- Webserver ---
    let app: Router = web::router(state.clone());

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("web server listening on {addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await?;

    // Ensure background tasks are signaled even if server exits unexpectedly.
    if state.request_shutdown() {
        log::info!("shutdown requested; draining background tasks");
    } else {
        log::info!("shutdown already in progress; draining background tasks");
    }

    let telemetry_shutdown_timeout = Duration::from_secs(20);
    let task_shutdown_timeout = Duration::from_secs(5);
    match tokio::time::timeout(telemetry_shutdown_timeout, &mut tt).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("telemetry task ended with error: {e}"),
        Err(_) => log::error!(
            "telemetry task did not shut down within {:?}",
            telemetry_shutdown_timeout
        ),
    }
    if !tt.is_finished() {
        tt.abort();
        let _ = tt.await;
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut st).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("safety task ended with error: {e}"),
        Err(_) => log::error!(
            "safety task did not shut down within {:?}",
            task_shutdown_timeout
        ),
    }
    if !st.is_finished() {
        st.abort();
        let _ = st.await;
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut sequence_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("sequence task ended with error: {e}"),
        Err(_) => log::error!(
            "sequence task did not shut down within {:?}",
            task_shutdown_timeout
        ),
    }
    if !sequence_task.is_finished() {
        sequence_task.abort();
        let _ = sequence_task.await;
    }

    let db_drain_timeout = Duration::from_secs(10);
    if !state.wait_for_db_writes(db_drain_timeout).await {
        log::error!(
            "Timed out waiting for DB writes. Pending writes remaining: {}",
            state.pending_db_write_count()
        );
    }

    let telemetry_db = state.telemetry_db_pool();
    let telemetry_db_path = state.telemetry_db_path();
    if telemetry_db_path == "sqlite::memory:" {
        telemetry_db.close().await;
    } else {
        close_and_finalize_sqlite(telemetry_db, &telemetry_db_path).await;
    }

    close_and_finalize_sqlite(state.auth_db.clone(), &auth_db_path_str).await;
    if let Err(err) = state.gpio.reset_outputs_low() {
        log::error!("failed to reset GPIO outputs low during shutdown: {err}");
    }
    log::info!("groundstation backend shutdown complete");
    Ok(())
}
