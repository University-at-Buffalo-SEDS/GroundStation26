// main.rs

mod auth;
#[cfg(feature = "testing")]
mod dummy_packets;
mod flight_sim;
mod gpio;
mod gpio_panel;
mod layout;
mod loadcell;
mod map;
mod radio;
mod radio_config;
mod ring_buffer;
mod rocket_commands;
mod safety_task;
mod sequences;
mod state;
mod telemetry_task;
mod types;
mod web;

use crate::map::{ensure_map_data, DEFAULT_MAP_REGION};
use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::sequences::{default_action_policy, start_sequence_task};
use crate::state::{AppState, BoardStatus};
use crate::telemetry_task::{get_current_timestamp_ms, set_network_time_router, telemetry_task};

#[cfg(any(feature = "testing", feature = "hitl_mode"))]
use crate::radio::DummyRadio;
use crate::radio::{link_description, open_link, startup_failure_hint, RadioDevice};
use crate::types::{Board, FlightState as FlightStateMode};
use axum::Router;
use sedsprintf_rs_2026::config::DataEndpoint::{Abort, FlightState, GroundStation};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::packet::Packet;
use sedsprintf_rs_2026::router::{EndpointHandler, RouterMode};
use sedsprintf_rs_2026::timesync::{TimeSyncConfig, TimeSyncRole};
use sedsprintf_rs_2026::TelemetryError;
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Row};
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

fn env_i64(name: &str, default: i64, min: i64, max: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn ensure_sqlite_db_file(path: &Path) -> anyhow::Result<String> {
    if !path.exists() {
        fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
        fs::write(path, b"")?;
        println!("Created empty DB file: {}", path.display());
    }
    Ok(fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string())
}

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

async fn apply_sqlite_pragmas(db: &sqlx::SqlitePool) {
    let synchronous = std::env::var("GS_SQLITE_SYNCHRONOUS")
        .unwrap_or_else(|_| "NORMAL".to_string())
        .to_uppercase();
    let synchronous = match synchronous.as_str() {
        "OFF" | "NORMAL" | "FULL" | "EXTRA" => synchronous,
        _ => "NORMAL".to_string(),
    };

    let busy_timeout_ms = env_i64("GS_SQLITE_BUSY_TIMEOUT_MS", 5_000, 100, 120_000);
    let wal_autocheckpoint = env_i64("GS_SQLITE_WAL_AUTOCHECKPOINT", 1_000, 100, 100_000);
    let cache_kib = env_i64("GS_SQLITE_CACHE_SIZE_KIB", 32 * 1024, 1024, 512 * 1024);
    let cache_pages = -cache_kib; // negative => kibibytes

    let pragmas = [
        "PRAGMA journal_mode=WAL;".to_string(),
        format!("PRAGMA synchronous={synchronous};"),
        "PRAGMA temp_store=MEMORY;".to_string(),
        format!("PRAGMA busy_timeout={busy_timeout_ms};"),
        format!("PRAGMA wal_autocheckpoint={wal_autocheckpoint};"),
        format!("PRAGMA cache_size={cache_pages};"),
    ];

    for stmt in pragmas {
        if let Err(err) = sqlx::query(&stmt).execute(db).await {
            eprintln!("SQLite pragma failed ({stmt}): {err}");
        }
    }
}

async fn flush_sqlite_journals(db: &sqlx::SqlitePool) {
    async fn exec_pragma_with_retry(
        db: &sqlx::SqlitePool,
        stmt: &str,
        retries: usize,
        delay_ms: u64,
    ) -> Result<(), sqlx::Error> {
        for attempt in 0..retries {
            match sqlx::query(stmt).execute(db).await {
                Ok(_) => return Ok(()),
                Err(err) => {
                    if attempt + 1 >= retries {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
        Ok(())
    }

    let retries = env_usize("GS_SQLITE_SHUTDOWN_PRAGMA_RETRIES", 8, 1, 60);
    let delay_ms = env_i64("GS_SQLITE_SHUTDOWN_PRAGMA_RETRY_DELAY_MS", 120, 10, 5_000) as u64;

    // If DB is in WAL mode, this checkpoints all frames and truncates WAL to 0 bytes.
    if let Err(err) =
        exec_pragma_with_retry(db, "PRAGMA wal_checkpoint(TRUNCATE);", retries, delay_ms).await
    {
        eprintln!("SQLite wal_checkpoint(TRUNCATE) failed after {retries} attempts: {err}");
    }

    // Optional lightweight cleanup/analysis pass.
    if let Err(err) = exec_pragma_with_retry(db, "PRAGMA optimize;", retries, delay_ms).await {
        eprintln!("SQLite PRAGMA optimize failed after {retries} attempts: {err}");
    }
}

async fn remove_sqlite_sidecars(db_path: &str) {
    let retries = env_usize("GS_SQLITE_SIDECAR_DELETE_RETRIES", 12, 1, 120);
    let retry_delay_ms = env_i64("GS_SQLITE_SIDECAR_DELETE_DELAY_MS", 100, 10, 2_000) as u64;
    for suffix in [".wal", ".shm", "-wal", "-shm", "-journal", ".journal"] {
        let sidecar = format!("{db_path}{suffix}");
        for attempt in 0..retries {
            match fs::remove_file(&sidecar) {
                Ok(()) => break,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                Err(err) => {
                    if attempt + 1 >= retries {
                        eprintln!("Failed removing SQLite sidecar {sidecar}: {err}");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }
    }
}

fn sqlite_sidecars_present(db_path: &str) -> Vec<String> {
    [".wal", ".shm", "-wal", "-shm", "-journal", ".journal"]
        .into_iter()
        .map(|suffix| format!("{db_path}{suffix}"))
        .filter(|p| Path::new(p).exists())
        .collect()
}

async fn finalize_sqlite_after_pool_close(db_path: &str) {
    let url = format!("sqlite://{db_path}");
    let mut conn = match SqliteConnection::connect(&url).await {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("Failed to reopen SQLite DB for finalization ({db_path}): {err}");
            return;
        }
    };

    for stmt in [
        "PRAGMA busy_timeout=5000;",
        "PRAGMA wal_checkpoint(TRUNCATE);",
        "PRAGMA optimize;",
    ] {
        if let Err(err) = sqlx::query(stmt).execute(&mut conn).await {
            eprintln!("SQLite finalization pragma failed ({stmt}): {err}");
        }
    }

    let retries = env_usize("GS_SQLITE_SHUTDOWN_PRAGMA_RETRIES", 8, 1, 60);
    let retry_delay_ms = env_i64("GS_SQLITE_SHUTDOWN_PRAGMA_RETRY_DELAY_MS", 120, 10, 5_000) as u64;
    for attempt in 0..retries {
        match sqlx::query("PRAGMA journal_mode=DELETE;")
            .execute(&mut conn)
            .await
        {
            Ok(_) => break,
            Err(err) => {
                if attempt + 1 >= retries {
                    eprintln!(
                        "SQLite finalization pragma failed (PRAGMA journal_mode=DELETE;): {err}"
                    );
                } else {
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms)).await;
                }
            }
        }
    }

    if let Err(err) = conn.close().await {
        eprintln!("Failed closing SQLite finalization connection: {err}");
    }
}

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
    let db_path = PathBuf::from("./data/groundstation.db");
    let db_path_str = ensure_sqlite_db_file(&db_path)?;
    let auth_db_path = db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("users.db");
    let auth_db_path_str = ensure_sqlite_db_file(&auth_db_path)?;

    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path_str)).await?;
    apply_sqlite_pragmas(&db).await;
    let auth_db = sqlx::SqlitePool::connect(&format!("sqlite://{}", auth_db_path_str)).await?;
    apply_sqlite_pragmas(&auth_db).await;

    // --- Tables ---
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
            sender_id    TEXT,
            values_json  TEXT,
            payload_json TEXT
        );
        "#,
    )
        .execute(&db)
        .await?;

    // Add values_json column for older DBs.
    let cols = sqlx::query("PRAGMA table_info(telemetry)")
        .fetch_all(&db)
        .await?;
    let has_values_json = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "values_json");
    if !has_values_json {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN values_json TEXT")
            .execute(&db)
            .await?;
    }
    let has_payload_json = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "payload_json");
    if !has_payload_json {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN payload_json TEXT")
            .execute(&db)
            .await?;
    }
    let has_sender_id = cols
        .iter()
        .any(|row| row.get::<String, _>("name") == "sender_id");
    if !has_sender_id {
        sqlx::query("ALTER TABLE telemetry ADD COLUMN sender_id TEXT")
            .execute(&db)
            .await?;
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS alerts (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            severity     TEXT    NOT NULL, -- 'warning' or 'error'
            message      TEXT    NOT NULL
        );
        "#,
    )
        .execute(&db)
        .await?;

    ensure_auth_sessions_table(&auth_db).await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS flight_state (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            f_state      INTEGER NOT NULL
        );
        "#,
    )
        .execute(&db)
        .await?;

    // --- Channels ---
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let ws_broadcast_capacity = env_usize("GS_WS_BROADCAST_CAPACITY", 8192, 512, 262_144);
    let board_status_capacity = env_usize("GS_BOARD_STATUS_BROADCAST_CAPACITY", 256, 64, 4096);
    let alerts_capacity = env_usize("GS_ALERTS_BROADCAST_CAPACITY", 1024, 128, 8192);
    let notifications_capacity = env_usize("GS_NOTIFICATIONS_BROADCAST_CAPACITY", 64, 16, 2048);
    let actions_capacity = env_usize("GS_ACTION_POLICY_BROADCAST_CAPACITY", 64, 16, 2048);
    let (ws_tx, _ws_rx) = broadcast::channel(ws_broadcast_capacity);
    let (board_status_tx, _board_status_rx) = broadcast::channel(board_status_capacity);
    let (notifications_tx, _notifications_rx) = broadcast::channel(notifications_capacity);
    let (action_policy_tx, _action_policy_rx) = broadcast::channel(actions_capacity);
    let (shutdown_tx, _shutdown_rx) = broadcast::channel(8);

    // --- Shared state ---
    let mut board_status = HashMap::new();
    for board in Board::ALL {
        board_status.insert(
            *board,
            BoardStatus {
                last_seen_ms: None,
                ema_gap_ms: None,
                warned: false,
            },
        );
    }

    let ring_buffer_capacity = env_usize("GS_RING_BUFFER_CAPACITY", 65_536, 1024, 1_000_000);
    let loadcell_calibration = loadcell::load_or_default();
    let radio_links = radio_config::load_or_default();
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
        db,
        auth_db,
        state: Arc::new(Mutex::new(FlightStateMode::Startup)),
        state_tx: broadcast::channel(16).0,
        gpio,
        board_status: Arc::new(Mutex::new(board_status)),
        board_status_tx,
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
        action_policy: Arc::new(Mutex::new(default_action_policy())),
        action_policy_tx,
        last_command_ms: Arc::new(Mutex::new(HashMap::new())),
        recent_telemetry_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        latest_gps_fix_by_sender: Arc::new(Mutex::new(HashMap::new())),
        latest_gps_satellites_by_sender: Arc::new(Mutex::new(HashMap::new())),
        recent_alerts_cache: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        av_bay_radio_connected: Arc::new(AtomicBool::new(false)),
        fill_radio_connected: Arc::new(AtomicBool::new(false)),
        topology_router: Arc::new(std::sync::OnceLock::new()),
        auth,
    });

    gpio_panel::setup_gpio_panel(state.clone()).expect("failed to setup gpio panel");
    let sequence_shutdown_rx = state.shutdown_subscribe();
    let mut sequence_task = start_sequence_task(state.clone(), sequence_shutdown_rx);

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let abort_handler_state_clone = state.clone();
    let flight_state_handler_state_clone = state.clone();

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

    let mut cfg = sedsprintf_rs_2026::router::RouterConfig::new([
        ground_station_handler,
        abort_handler,
        flight_state_handler,
    ]);
    if telemetry_task::timesync_enabled() {
        cfg = cfg.with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Auto,
            priority: 50,
            source_timeout_ms: 5_000,
            announce_interval_ms: 1_000,
            request_interval_ms: 1_000,
        });
    }

    // --- Radios ---
    println!("AV bay config: {}", link_description(&radio_links.av_bay));
    println!(
        "Fill box config: {}",
        link_description(&radio_links.fill_box)
    );

    let (rocket_radio, av_bay_radio_connected): (Arc<Mutex<Box<dyn RadioDevice>>>, bool) =
        match open_link(&radio_links.av_bay) {
            Ok(r) => {
                println!("Rocket radio online");
                (Arc::new(Mutex::new(r)), true)
            }
            Err(e) => {
                println!("Rocket radio missing, using DummyRadio: {}", e);
                eprintln!(
                    "AV bay link setup hint: {}",
                    startup_failure_hint(&radio_links.av_bay)
                );
                #[cfg(feature = "testing")]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyRadio::new("Rocket Radio")))),
                        false,
                    )
                }
                #[cfg(all(not(feature = "testing"), feature = "hitl_mode"))]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyRadio::new("Rocket Radio")))),
                        false,
                    )
                }
                #[cfg(not(feature = "testing"))]
                #[cfg(not(feature = "hitl_mode"))]
                panic!("Rocket radio missing and testing mode not enabled")
            }
        };

    let (umbilical_radio, fill_radio_connected): (Arc<Mutex<Box<dyn RadioDevice>>>, bool) =
        match open_link(&radio_links.fill_box) {
            Ok(r) => {
                println!("Umbilical radio online");
                (Arc::new(Mutex::new(r)), true)
            }
            Err(e) => {
                println!("Umbilical radio missing, using DummyRadio: {}", e);
                eprintln!(
                    "Fill box link setup hint: {}",
                    startup_failure_hint(&radio_links.fill_box)
                );
                #[cfg(feature = "testing")]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyRadio::new("Umbilical Radio")))),
                        false,
                    )
                }
                #[cfg(all(not(feature = "testing"), feature = "hitl_mode"))]
                {
                    (
                        Arc::new(Mutex::new(Box::new(DummyRadio::new("Umbilical Radio")))),
                        false,
                    )
                }
                #[cfg(not(feature = "testing"))]
                #[cfg(not(feature = "hitl_mode"))]
                panic!("Umbilical radio missing and testing mode not enabled")
            }
        };
    state
        .av_bay_radio_connected
        .store(av_bay_radio_connected, Ordering::Relaxed);
    state
        .fill_radio_connected
        .store(fill_radio_connected, Ordering::Relaxed);

    let router = Arc::new(sedsprintf_rs_2026::router::Router::new(
        RouterMode::Relay,
        cfg,
    ));
    set_network_time_router(router.clone());
    let _ = state.topology_router.set(router.clone());

    let rocket_side = {
        let rocket_radio = Arc::clone(&rocket_radio);
        router.add_side_serialized("rocket_radio", move |pkt| {
            let mut guard = rocket_radio
                .lock()
                .map_err(|_| TelemetryError::HandlerError("Radio mutex poisoned"))?;
            guard
                .send_data(pkt)
                .map_err(|_| TelemetryError::HandlerError("Tx Handler failed"))?;
            Ok(())
        })
    };

    let umbilical_side = {
        let umbilical_radio = Arc::clone(&umbilical_radio);
        router.add_side_serialized("umbilical_radio", move |pkt| {
            let mut guard = umbilical_radio
                .lock()
                .map_err(|_| TelemetryError::HandlerError("Radio mutex poisoned"))?;
            guard
                .send_data(pkt)
                .map_err(|_| TelemetryError::HandlerError("Tx Handler failed"))?;
            Ok(())
        })
    };

    rocket_radio
        .lock()
        .expect("failed to get rocket radio lock")
        .set_side_id(rocket_side);
    umbilical_radio
        .lock()
        .expect("failed to get umbilical radio lock")
        .set_side_id(umbilical_side);

    router.log_queue(DataType::MessageData, "hello".as_bytes())?;
    router.log_queue(DataType::FlightState, &[FlightStateMode::Startup as u8])?;

    // --- Background tasks ---
    let telemetry_shutdown_rx = state.shutdown_subscribe();
    let safety_shutdown_rx = state.shutdown_subscribe();
    let mut tt = tokio::spawn(telemetry_task(
        state.clone(),
        router.clone(),
        vec![rocket_radio, umbilical_radio],
        cmd_rx,
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

    flush_sqlite_journals(&state.db).await;
    state.db.close().await;
    finalize_sqlite_after_pool_close(&db_path_str).await;
    remove_sqlite_sidecars(&db_path_str).await;
    let lingering = sqlite_sidecars_present(&db_path_str);
    if !lingering.is_empty() {
        eprintln!(
            "WARNING: SQLite sidecar files still present after shutdown cleanup: {}",
            lingering.join(", ")
        );
    }

    flush_sqlite_journals(&state.auth_db).await;
    state.auth_db.close().await;
    finalize_sqlite_after_pool_close(&auth_db_path_str).await;
    remove_sqlite_sidecars(&auth_db_path_str).await;
    let lingering = sqlite_sidecars_present(&auth_db_path_str);
    if !lingering.is_empty() {
        eprintln!(
            "WARNING: Auth SQLite sidecar files still present after shutdown cleanup: {}",
            lingering.join(", ")
        );
    }
    Ok(())
}
