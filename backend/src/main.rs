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
mod flight_setup;
mod flight_sim;
mod gpio;
mod gpio_panel;
mod i18n;
mod layout;
mod loadcell;
mod map;
mod ring_buffer;
mod rocket_commands;
mod safety_task;
mod sequences;
mod state;
mod telemetry_db;
mod telemetry_task;
mod types;
mod web;

use crate::map::{DEFAULT_MAP_REGION, ensure_map_data};
use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::sequences::{default_action_policy, start_sequence_task};
use crate::state::{AppState, BoardStatus};
use crate::telemetry_db::{
    DEFAULT_TELEMETRY_DB_FILENAME, DbQueueItem, LaunchClockMsg, RecordingModeWire,
    RecordingStatusMsg, apply_sqlite_pragmas, close_and_finalize_sqlite, ensure_sqlite_db_file,
    open_telemetry_db,
};
use crate::telemetry_task::{
    CommsWorkerHandle, get_current_timestamp_ms, set_network_time_router, telemetry_task,
};

#[cfg(any(feature = "testing", feature = "hitl_mode", feature = "test_fire_mode"))]
use crate::comms::DummyComms;
use crate::comms::{CommsDevice, link_description, open_link, startup_failure_hint};
use crate::comms_config::{CommsLinkConfig, SerialProtocol};
use crate::types::{Board, FlightState as FlightStateMode};
use axum::Router;
use sedsprintf_rs_2026::TelemetryError;
use sedsprintf_rs_2026::config::DataEndpoint::{Abort, FlightState, GroundStation, HeartBeat};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::{EndpointHandler, RouterMode, RouterSideOptions};
use sedsprintf_rs_2026::timesync::{TimeSyncConfig, TimeSyncRole};
use sqlx::Row;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Duration;

use crate::web::emit_error;
use tokio::sync::{broadcast, mpsc};

fn env_usize(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
        .clamp(min, max)
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

fn router_hop_reliable_enabled(link: &CommsLinkConfig) -> bool {
    match link {
        CommsLinkConfig::I2c { .. } => false,
        CommsLinkConfig::Serial { serial }
        | CommsLinkConfig::RaspberryPiGpioUart { serial }
        | CommsLinkConfig::CustomSerial { serial } => {
            !matches!(serial.protocol, SerialProtocol::RawUart)
        }
        CommsLinkConfig::Spi { .. } | CommsLinkConfig::Can { .. } => true,
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

async fn load_persisted_messages(
    db: &sqlx::SqlitePool,
) -> anyhow::Result<Vec<crate::sequences::PersistentNotification>> {
    let rows = sqlx::query(
        r#"
        SELECT id, timestamp_ms, message, action_label, action_cmd
        FROM messages
        ORDER BY timestamp_ms DESC, id DESC
        LIMIT 200
        "#,
    )
    .fetch_all(db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| crate::sequences::PersistentNotification {
            id: row.get::<i64, _>("id") as u64,
            timestamp_ms: row.get("timestamp_ms"),
            message: row.get("message"),
            persistent: false,
            action_label: row.get("action_label"),
            action_cmd: row.get("action_cmd"),
        })
        .collect())
}

/// Waits for process termination signals and then fan-outs the app-wide shutdown request.
async fn shutdown_signal(state: Arc<AppState>) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            eprintln!("Failed to install Ctrl+C handler: {err}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                eprintln!("Failed to install SIGTERM handler: {err}");
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

    state.request_shutdown();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize GPIO
    let gpio = gpio::GpioPins::new();

    // Ensure offline map tiles
    if let Err(e) = ensure_map_data(DEFAULT_MAP_REGION).await {
        eprintln!("WARNING: failed to ensure map tiles: {e:#}");
        // you can choose to return Err(e) instead if tiles are mandatory
    }

    // --- DB path ---
    let db_path = PathBuf::from("./data").join(DEFAULT_TELEMETRY_DB_FILENAME);
    let (db, db_path_str) = open_telemetry_db(&db_path).await?;
    let auth_db_path = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("users.db");
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
        dashboard_reset_tx,
        db: Arc::new(Mutex::new(db)),
        db_path: Arc::new(Mutex::new(db_path_str.clone())),
        placeholder_db_path: db_path_str.clone(),
        db_queue_tx,
        auth_db,
        state: Arc::new(Mutex::new(FlightStateMode::Startup)),
        state_tx: broadcast::channel(16).0,
        gpio,
        board_status: Arc::new(Mutex::new(board_status)),
        board_status_tx,
        last_board_status_broadcast_ms: Arc::new(AtomicU64::new(0)),
        last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
        umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
        latest_fill_mass_kg: Arc::new(Mutex::new(None)),
        loadcell_calibration: Arc::new(Mutex::new(loadcell_calibration)),
        shutdown_tx,
        pending_db_writes: Arc::new(AtomicUsize::new(0)),
        db_write_notify: Arc::new(Notify::new()),
        notifications: Arc::new(Mutex::new(Vec::new())),
        notifications_tx,
        next_notification_id: Arc::new(AtomicU64::new(0)),
        messages: Arc::new(Mutex::new(Vec::new())),
        messages_tx,
        next_message_id: Arc::new(AtomicU64::new(0)),
        action_policy: Arc::new(Mutex::new(default_action_policy())),
        action_policy_tx,
        fill_targets: Arc::new(Mutex::new(fill_targets::load_or_default())),
        fill_targets_tx,
        launch_clock: Arc::new(Mutex::new(LaunchClockMsg::idle())),
        launch_clock_tx,
        recording_status: Arc::new(Mutex::new(RecordingStatusMsg {
            mode: RecordingModeWire::Idle,
            db_path: Some(db_path_str.clone()),
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

    let persisted_messages = load_persisted_messages(&state.telemetry_db_pool()).await?;
    state.set_messages_snapshot(persisted_messages);

    gpio_panel::setup_gpio_panel(state.clone()).expect("failed to setup gpio panel");
    let sequence_shutdown_rx = state.shutdown_subscribe();
    let mut sequence_task = start_sequence_task(state.clone(), sequence_shutdown_rx);

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let abort_handler_state_clone = state.clone();
    let flight_state_handler_state_clone = state.clone();
    let heartbeat_handler_state_clone = state.clone();

    let ground_station_handler =
        EndpointHandler::new_packet_handler(GroundStation, move |pkt: &Packet| {
            ground_station_handler_state_clone
                .mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            ground_station_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = ground_station_handler_state_clone
                .ring_buffer
                .lock()
                .unwrap();
            rb.push(pkt.clone());
            Ok(())
        });

    let flight_state_handler =
        EndpointHandler::new_packet_handler(FlightState, move |pkt: &Packet| {
            flight_state_handler_state_clone
                .mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            flight_state_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = flight_state_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(pkt.clone());
            Ok(())
        });

    let abort_handler = EndpointHandler::new_packet_handler(Abort, move |pkt: &Packet| {
        abort_handler_state_clone.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
        abort_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
        let error_msg = pkt
            .data_as_string()
            .expect("Abort packet with invalid UTF-8");
        emit_error(&abort_handler_state_clone, error_msg);
        Ok(())
    });

    let heartbeat_handler = EndpointHandler::new_packet_handler(HeartBeat, move |pkt: &Packet| {
        heartbeat_handler_state_clone.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
        heartbeat_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
        let mut rb = heartbeat_handler_state_clone.ring_buffer.lock().unwrap();
        rb.push(pkt.clone());
        Ok(())
    });

    let mut cfg = sedsprintf_rs_2026::router::RouterConfig::new([
        ground_station_handler,
        abort_handler,
        flight_state_handler,
        heartbeat_handler,
    ]);
    if telemetry_task::timesync_enabled() {
        cfg = cfg.with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Auto,
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

    let (rocket_comms, av_bay_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        match open_link(&comms_links.av_bay) {
            Ok(r) => {
                gs_debug_println!("Rocket comms online");
                (Arc::new(Mutex::new(r)), true)
            }
            Err(e) => {
                gs_debug_println!("Rocket comms missing, using DummyComms: {}", e);
                eprintln!(
                    "AV bay link setup hint: {}",
                    startup_failure_hint(&comms_links.av_bay)
                );
                #[cfg(feature = "testing")]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyComms::new("Rocket Comms")))),
                        false,
                    )
                }
                #[cfg(all(
                    not(feature = "testing"),
                    any(feature = "hitl_mode", feature = "test_fire_mode")
                ))]
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
        };

    let (umbilical_comms, fill_comms_connected): (Arc<Mutex<Box<dyn CommsDevice>>>, bool) =
        match open_link(&comms_links.fill_box) {
            Ok(r) => {
                gs_debug_println!("Umbilical comms online");
                (Arc::new(Mutex::new(r)), true)
            }
            Err(e) => {
                gs_debug_println!("Umbilical comms missing, using DummyComms: {}", e);
                eprintln!(
                    "Fill box link setup hint: {}",
                    startup_failure_hint(&comms_links.fill_box)
                );
                #[cfg(feature = "testing")]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyComms::new("Umbilical Comms")))),
                        false,
                    )
                }
                #[cfg(all(
                    not(feature = "testing"),
                    any(feature = "hitl_mode", feature = "test_fire_mode")
                ))]
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
        };
    state
        .av_bay_comms_connected
        .store(av_bay_comms_connected, Ordering::Relaxed);
    state
        .fill_comms_connected
        .store(fill_comms_connected, Ordering::Relaxed);

    let router = Arc::new(sedsprintf_rs_2026::router::Router::new(
        RouterMode::Relay,
        cfg,
    ));
    set_network_time_router(router.clone());
    let _ = state.topology_router.set(router.clone());

    let (rocket_tx, rocket_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (umbilical_tx, umbilical_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let rocket_side = {
        let rocket_tx = rocket_tx.clone();
        let opts = RouterSideOptions {
            reliable_enabled: router_hop_reliable_enabled(&comms_links.av_bay),
            link_local_enabled: false,
        };
        router.add_side_serialized_with_options(
            "rocket_comms",
            move |pkt| {
                rocket_tx
                    .send(pkt.to_vec())
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
            // The Pico bridge on the I2C side needs router-local packets (for example
            // GroundStation-addressed traffic and local heartbeat/discovery flow) to traverse
            // the physical link so it can forward them back out over its UART/USB bridge.
            link_local_enabled: true,
        };
        router.add_side_serialized_with_options(
            "umbilical_comms",
            move |pkt| {
                umbilical_tx
                    .send(pkt.to_vec())
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

    if let Err(err) = router.announce_discovery() {
        eprintln!("WARNING: failed to queue initial discovery announce: {err}");
    }

    router.log_queue(DataType::FlightState, &[FlightStateMode::Startup as u8])?;

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
                dedicated_radio_io: true,
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
    let mut st = tokio::spawn(safety_task(
        state.clone(),
        router.clone(),
        safety_shutdown_rx,
    ));

    // --- Webserver ---
    let app: Router = web::router(state.clone());

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.clone()))
        .await?;

    // Ensure background tasks are signaled even if server exits unexpectedly.
    state.request_shutdown();

    let telemetry_shutdown_timeout = Duration::from_secs(20);
    let task_shutdown_timeout = Duration::from_secs(5);
    match tokio::time::timeout(telemetry_shutdown_timeout, &mut tt).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Telemetry task ended with error: {e}"),
        Err(_) => eprintln!(
            "Telemetry task did not shut down within {:?}",
            telemetry_shutdown_timeout
        ),
    }
    if !tt.is_finished() {
        tt.abort();
        let _ = tt.await;
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut st).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Safety task ended with error: {e}"),
        Err(_) => eprintln!(
            "Safety task did not shut down within {:?}",
            task_shutdown_timeout
        ),
    }
    if !st.is_finished() {
        st.abort();
        let _ = st.await;
    }
    match tokio::time::timeout(task_shutdown_timeout, &mut sequence_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Sequence task ended with error: {e}"),
        Err(_) => eprintln!(
            "Sequence task did not shut down within {:?}",
            task_shutdown_timeout
        ),
    }
    if !sequence_task.is_finished() {
        sequence_task.abort();
        let _ = sequence_task.await;
    }

    let db_drain_timeout = Duration::from_secs(10);
    if !state.wait_for_db_writes(db_drain_timeout).await {
        eprintln!(
            "Timed out waiting for DB writes. Pending writes remaining: {}",
            state.pending_db_write_count()
        );
    }

    let telemetry_db = state.telemetry_db_pool();
    let telemetry_db_path = state.telemetry_db_path();
    close_and_finalize_sqlite(telemetry_db, &telemetry_db_path).await;

    close_and_finalize_sqlite(state.auth_db.clone(), &auth_db_path_str).await;
    Ok(())
}
