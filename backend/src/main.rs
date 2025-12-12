// main.rs

#[cfg(feature = "testing")]
mod dummy_packets;
mod gpio;
mod map;
mod radio;
mod ring_buffer;
mod safety_task;
mod state;
mod telemetry_task;
mod web;

use crate::map::{ensure_map_data, DEFAULT_MAP_REGION};
use crate::ring_buffer::RingBuffer;
use crate::safety_task::safety_task;
use crate::state::AppState;
use crate::telemetry_task::{get_current_timestamp_ms, telemetry_task};

use crate::gpio::Trigger::RisingEdge;
#[cfg(feature = "testing")]
use crate::radio::DummyRadio;
use crate::radio::{Radio, RadioDevice, RADIO_BAUDRATE, ROCKET_RADIO_PORT, UMBILICAL_RADIO_PORT};
use crate::web::emit_error;
use axum::Router;
use groundstation_shared::FlightState;
use sedsprintf_rs_2026::config::DataEndpoint::{Abort, GroundStation};
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::EndpointHandler;
use sedsprintf_rs_2026::telemetry_packet::TelemetryPacket;
use sedsprintf_rs_2026::{TelemetryError, TelemetryResult};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

fn clock() -> Box<dyn sedsprintf_rs_2026::router::Clock + Send + Sync> {
    Box::new(get_current_timestamp_ms)
}

const GPIO_IGNITION_PIN: u8 = 5;
const GPIO_ABORT_PIN: u8 = 9;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize GPIO
    let gpio = gpio::GpioPins::new();

    gpio.setup_input_pin(GPIO_ABORT_PIN)
        .expect("failed to setup gpio pin");
    gpio.setup_output_pin(GPIO_IGNITION_PIN)
        .expect("failed to setup gpio pin");

    let gpio_clone = gpio.clone();

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

    // --- Tables ---
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS telemetry (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp_ms INTEGER NOT NULL,
            data_type    TEXT    NOT NULL,
            v0           REAL,
            v1           REAL,
            v2           REAL,
            v3           REAL,
            v4           REAL,
            v5           REAL,
            v6           REAL,
            v7           REAL
        );
        "#,
    )
    .execute(&db)
    .await?;

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
    let (ws_tx, _ws_rx) = broadcast::channel(512);

    // --- Shared state ---
    let state = Arc::new(AppState {
        ring_buffer: Arc::new(Mutex::new(RingBuffer::new(1024))),
        cmd_tx,
        ws_tx,
        warnings_tx: broadcast::channel(256).0,
        errors_tx: broadcast::channel(256).0,
        db,
        state: Arc::new(Mutex::new(FlightState::Startup)),
        state_tx: broadcast::channel(16).0,
        gpio,
    });

    // --- Router endpoint handlers ---
    let ground_station_handler_state_clone = state.clone();
    let abort_handler_state_clone = state.clone();

    let ground_station_handler =
        EndpointHandler::new_packet_handler(GroundStation, move |pkt: &TelemetryPacket| {
            let mut rb = ground_station_handler_state_clone
                .ring_buffer
                .lock()
                .unwrap();
            rb.push(pkt.clone());
            Ok(())
        });

    let abort_handler = EndpointHandler::new_packet_handler(Abort, move |pkt: &TelemetryPacket| {
        let error_msg = pkt
            .data_as_string()
            .expect("Abort packet with invalid UTF-8");
        emit_error(&abort_handler_state_clone, error_msg);
        Ok(())
    });

    let cfg = sedsprintf_rs_2026::router::BoardConfig::new([ground_station_handler, abort_handler]);

    // --- Radios ---
    let rocket_radio: Arc<Mutex<Box<dyn RadioDevice>>> =
        match Radio::open(ROCKET_RADIO_PORT, RADIO_BAUDRATE) {
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
        match Radio::open(UMBILICAL_RADIO_PORT, RADIO_BAUDRATE) {
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

    let serialized_handler = {
        let rocket_radio: Arc<Mutex<Box<dyn RadioDevice>>> = Arc::clone(&rocket_radio);
        let umbilical_radio: Arc<Mutex<Box<dyn RadioDevice>>> = Arc::clone(&umbilical_radio);
        Some(move |pkt: &[u8]| -> TelemetryResult<()> {
            let mut guard = rocket_radio
                .lock()
                .map_err(|_| TelemetryError::HandlerError("Radio mutex poisoned"))?;
            guard
                .send_data(pkt)
                .map_err(|_| TelemetryError::HandlerError("Tx Handler failed"))?;
            
            let mut guard = umbilical_radio
                .lock()
                .map_err(|_| TelemetryError::HandlerError("Radio mutex poisoned"))?;
            guard
                .send_data(pkt)
                .map_err(|_| TelemetryError::HandlerError("Tx Handler failed"))?;
            Ok(())
        })
    };

    let router = Arc::new(sedsprintf_rs_2026::router::Router::new(
        serialized_handler,
        cfg,
        clock(),
    ));

    // Clone what you need for the callback
    let router_for_cb = router.clone();
    let state_for_cb = state.clone();

    gpio_clone
        .setup_callback_input_pin(
            GPIO_ABORT_PIN,
            RisingEdge,
            Duration::from_millis(50),
            move |_| {
                // now we use the owned clones captured by `move`
                router_for_cb
                    .log::<u8>(DataType::Abort, "Manual abort button pressed!".as_bytes())
                    .expect("failed to log Abort command");

                emit_error(&state_for_cb, "Manual abort button pressed!".to_string());

                println!("Manual abort button pressed!");
            },
        )
        .expect("failed to setup gpio callback input");

    router.log_queue(DataType::MessageData, "hello".as_bytes())?;
    router.log_queue(DataType::FlightState, &[FlightState::Startup as u8])?;

    // --- Background tasks ---
    let _tt = tokio::spawn(telemetry_task(state.clone(), router.clone(), vec!(rocket_radio, umbilical_radio), cmd_rx));
    let _st = tokio::spawn(safety_task(state.clone(), router.clone()));

    // --- Webserver ---
    let app: Router = web::router(state);

    let addr = "0.0.0.0:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
