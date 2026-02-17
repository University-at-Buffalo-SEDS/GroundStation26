// main.rs

#[cfg(feature = "testing")]
mod dummy_packets;
mod gpio;
mod gpio_panel;
mod layout;
mod map;
mod radio;
mod ring_buffer;
mod rocket_commands;
mod safety_task;
mod state;
mod telemetry_task;
mod web;

use crate::map::{ensure_map_data, DEFAULT_MAP_REGION};
use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::state::{AppState, BoardStatus};
use crate::telemetry_task::{get_current_timestamp_ms, telemetry_task};

#[cfg(feature = "testing")]
use crate::radio::DummyRadio;
use crate::radio::{Radio, RadioDevice, RADIO_BAUD_RATE, ROCKET_RADIO_PORT, UMBILICAL_RADIO_PORT};
use axum::Router;
use groundstation_shared::{Board, FlightState as FlightStateMode};
use sedsprintf_rs_2026::config::DataEndpoint::{Abort, FlightState, GroundStation};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::{EndpointHandler, RouterMode};
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::TelemetryError;
use sqlx::Row;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Duration;

use crate::web::emit_error;
use tokio::sync::{broadcast, mpsc};

fn clock() -> Box<dyn sedsprintf_rs_2026::router::Clock + Send + Sync> {
    Box::new(get_current_timestamp_ms)
}

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
    let cache_kib = env_i64("GS_SQLITE_CACHE_SIZE_KIB", 32 * 1024, 1 * 1024, 512 * 1024);
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
    // If DB is in WAL mode, this checkpoints all frames and truncates WAL to 0 bytes.
    if let Err(err) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
        .execute(db)
        .await
    {
        eprintln!("SQLite wal_checkpoint(TRUNCATE) failed: {err}");
    }

    // Optional lightweight cleanup/analysis pass.
    if let Err(err) = sqlx::query("PRAGMA optimize;").execute(db).await {
        eprintln!("SQLite PRAGMA optimize failed: {err}");
    }

    // On shutdown, switch out of WAL to reduce chance of lingering -wal/-shm files.
    let switch_to_delete = std::env::var("GS_SQLITE_SHUTDOWN_JOURNAL_DELETE")
        .ok()
        .as_deref()
        != Some("0");
    if switch_to_delete
        && let Err(err) = sqlx::query("PRAGMA journal_mode=DELETE;").execute(db).await
    {
        eprintln!("SQLite PRAGMA journal_mode=DELETE failed: {err}");
    }
}

async fn remove_sqlite_sidecars(db_path: &str) {
    let retries = env_usize("GS_SQLITE_SIDECAR_DELETE_RETRIES", 12, 1, 120);
    let retry_delay_ms = env_i64("GS_SQLITE_SIDECAR_DELETE_DELAY_MS", 100, 10, 2_000) as u64;
    for suffix in [".wal", ".shm", "-wal", "-shm", "-journal", ".journal"] {
        let sidecar = format!("{db_path}{suffix}");
        for attempt in 0..retries {
            match std::fs::remove_file(&sidecar) {
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
    let db_path = "./data/groundstation.db";
    if !Path::new(db_path).exists() {
        fs::create_dir_all("./data")?;
        fs::write(db_path, b"")?;
        println!("Created empty DB file.");
    }

    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path)).await?;
    apply_sqlite_pragmas(&db).await;

    // --- Tables ---
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
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
    let (ws_tx, _ws_rx) = broadcast::channel(ws_broadcast_capacity);
    let (board_status_tx, _board_status_rx) = broadcast::channel(board_status_capacity);
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
    let state = Arc::new(AppState {
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(ring_buffer_capacity))),
        cmd_tx,
        ws_tx,
        warnings_tx: broadcast::channel(alerts_capacity).0,
        errors_tx: broadcast::channel(alerts_capacity).0,
        db,
        state: Arc::new(Mutex::new(FlightStateMode::Startup)),
        state_tx: broadcast::channel(16).0,
        gpio,
        board_status: Arc::new(Mutex::new(board_status)),
        board_status_tx,
        last_packet_rx_ms: Arc::new(AtomicU64::new(0)),
        umbilical_valve_states: Arc::new(Mutex::new(HashMap::new())),
        latest_fuel_tank_pressure: Arc::new(Mutex::new(None)),
        shutdown_tx,
        pending_db_writes: Arc::new(AtomicUsize::new(0)),
        db_write_notify: Arc::new(Notify::new()),
    });

    gpio_panel::setup_gpio_panel(state.clone()).expect("failed to setup gpio panel");

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let abort_handler_state_clone = state.clone();
    let flight_state_handler_state_clone = state.clone();

    let ground_station_handler =
        EndpointHandler::new_packet_handler(GroundStation, move |pkt: &TelemetryPacket| {
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
        EndpointHandler::new_packet_handler(FlightState, move |pkt: &TelemetryPacket| {
            flight_state_handler_state_clone
                .mark_board_seen(pkt.sender(), get_current_timestamp_ms());
            flight_state_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
            let mut rb = flight_state_handler_state_clone.ring_buffer.lock().unwrap();
            rb.push(pkt.clone());
            Ok(())
        });

    let abort_handler = EndpointHandler::new_packet_handler(Abort, move |pkt: &TelemetryPacket| {
        abort_handler_state_clone.mark_board_seen(pkt.sender(), get_current_timestamp_ms());
        abort_handler_state_clone.mark_packet_received(get_current_timestamp_ms());
        let error_msg = pkt
            .data_as_string()
            .expect("Abort packet with invalid UTF-8");
        emit_error(&abort_handler_state_clone, error_msg);
        Ok(())
    });

    let cfg = sedsprintf_rs_2026::router::RouterConfig::new([
        ground_station_handler,
        abort_handler,
        flight_state_handler,
    ]);

    // --- Radios ---
    let rocket_radio: Arc<Mutex<Box<dyn RadioDevice>>> =
        match Radio::open(ROCKET_RADIO_PORT, RADIO_BAUD_RATE) {
            Ok(r) => {
                println!("Rocket radio online");
                Arc::new(Mutex::new(Box::new(r)))
            }
            Err(e) => {
                println!("Rocket radio missing, using DummyRadio: {}", e);
                #[cfg(feature = "testing")]
                {
                    Arc::new(Mutex::new(Box::new(DummyRadio::new("Rocket Radio"))))
                }
                #[cfg(not(feature = "testing"))]
                panic!("Rocket radio missing and testing mode not enabled")
            }
        };

    let umbilical_radio: Arc<Mutex<Box<dyn RadioDevice>>> =
        match Radio::open(UMBILICAL_RADIO_PORT, RADIO_BAUD_RATE) {
            Ok(r) => {
                println!("Umbilical radio online");
                Arc::new(Mutex::new(Box::new(r)))
            }
            Err(e) => {
                println!("Umbilical radio missing, using DummyRadio: {}", e);
                #[cfg(feature = "testing")]
                {
                    Arc::new(Mutex::new(Box::new(DummyRadio::new("Umbilical Radio"))))
                }
                #[cfg(not(feature = "testing"))]
                panic!("Umbilical radio missing and testing mode not enabled")
            }
        };

    let router = Arc::new(sedsprintf_rs_2026::router::Router::new(
        RouterMode::Relay,
        cfg,
        clock(),
    ));

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
    let tt = tokio::spawn(telemetry_task(
        state.clone(),
        router.clone(),
        vec![rocket_radio, umbilical_radio],
        cmd_rx,
        telemetry_shutdown_rx,
    ));
    let st = tokio::spawn(safety_task(
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

    let task_shutdown_timeout = Duration::from_secs(5);
    match tokio::time::timeout(task_shutdown_timeout, tt).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Telemetry task ended with error: {e}"),
        Err(_) => eprintln!(
            "Telemetry task did not shut down within {:?}",
            task_shutdown_timeout
        ),
    }
    match tokio::time::timeout(task_shutdown_timeout, st).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("Safety task ended with error: {e}"),
        Err(_) => eprintln!(
            "Safety task did not shut down within {:?}",
            task_shutdown_timeout
        ),
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
    remove_sqlite_sidecars(db_path).await;
    Ok(())
}
