use crate::state::AppState;
use groundstation_shared::TelemetryRow;
use groundstation_shared::{u8_to_flight_state, TelemetryCommand};
use sedsprintf_rs_2026::config::DataType;

use crate::radio::RadioDevice;
use crate::web::{emit_warning, emit_warning_db_only, FlightStateMsg};
use groundstation_shared::Board;
use crate::GPIO_IGNITION_PIN;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<sedsprintf_rs_2026::router::Router>,
    radio: Vec<Arc<Mutex<Box<dyn RadioDevice>>>>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
) {
    let mut radio_interval = interval(Duration::from_millis(2));
    let mut handle_interval = interval(Duration::from_millis(1));
    let mut router_interval = interval(Duration::from_millis(10));
    let mut heartbeat_interval = interval(Duration::from_millis(500));
    let mut heartbeat_failed = false;

    loop {
        tokio::select! {
                _ = radio_interval.tick() => {
                    for radio in &radio {
                        match radio.lock().expect("failed to get lock").recv_packet(&router){
                            Ok(_) => {
                                // Packet received and handled by router
                            }
                            Err(e) => {
                                println!("radio_task exited with error: {}", e);
                            }
                        }
                    }
                }
            _= router_interval.tick() => {
                    router.process_all_queues_with_timeout(20).expect("Failed to process all queues with timeout");
                }
                Some(cmd) = rx.recv() => {
                    match cmd {
                        TelemetryCommand::Arm => {
                            router.log_queue(
                                    DataType::MessageData,
                                    "Arm".as_bytes()
                                ).expect("failed to log Arm command");
                            let gpio = &state.gpio;
                            gpio.write_output_pin(GPIO_IGNITION_PIN, true).expect("failed to set gpio output");
                            println!("Arm command sent");

                        }
                        TelemetryCommand::Disarm => {
                            router.log_queue(
                                    DataType::MessageData,
                                    "Disarm".as_bytes()
                                ).expect("failed to log Arm command");
                            {
                                let gpio = &state.gpio;
                                gpio.write_output_pin(GPIO_IGNITION_PIN, false).expect("failed to set gpio output");
                            }
                            println!("Disarm command sent");
                        }
                        TelemetryCommand::Abort => {
                            router.log(
                                    DataType::Abort,
                                    "Manual Abort Command Issued".as_ref(),
                                ).expect("failed to log Abort command");
                            println!("Abort command sent");
                        }
                        TelemetryCommand::Igniter => {
                            router.log_queue(
                                    DataType::ValveCommand,
                                    &[1u8],
                                ).expect("failed to log Igniter command");
                            println!("Igniter command sent");
                        }
                    TelemetryCommand::Pilot => {
                            router.log_queue(
                                    DataType::ValveCommand,
                                    &[2u8],
                                ).expect("failed to log Igniter command");
                            println!("Pilot command sent");
                        }
                    TelemetryCommand::Tanks => {
                            router.log_queue(
                                    DataType::ValveCommand,
                                    &[3u8],
                                ).expect("failed to log Igniter command");
                            println!("Tanks command sent");
                        }
                    }
                }
                _ = heartbeat_interval.tick() => {
                    if router.log_queue::<u8>(DataType::Heartbeat, &[]).is_ok() {
                        state.mark_board_seen(
                            Board::GroundStation.sender_id(),
                            get_current_timestamp_ms(),
                        );
                        heartbeat_failed = false;
                    } else if !heartbeat_failed {
                            emit_warning_db_only(
                                &state,
                                "Warning: Ground Station heartbeat send failed",
                            );
                            heartbeat_failed = true;
                
                    }
                }
                _ = handle_interval.tick() => {
                    handle_packet(&state).await;
                }
        }
    }
}

const DB_RETRIES: usize = 5;
const DB_RETRY_DELAY_MS: u64 = 50;

async fn insert_with_retry<F, Fut>(mut f: F) -> Result<(), sqlx::Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output=Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>>,
{
    let mut delay = DB_RETRY_DELAY_MS;
    let mut last_err: Option<sqlx::Error> = None;

    for _ in 0..=DB_RETRIES {
        match f().await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(1000);
            }
        }
    }

    Err(last_err.unwrap())
}

pub async fn handle_packet(state: &Arc<AppState>) {
    // Keep raw packet in ring buffer if you still want it
    let pkt = {
        //get the most recent packet from the ring buffer
        let mut rb = state.ring_buffer.lock().unwrap();
        match rb.pop_oldest() {
            Some(pkt) => pkt,
            None => return, // No packet to process
        }
    };

    state.mark_board_seen(pkt.sender(), get_current_timestamp_ms());

    if pkt.data_type() == DataType::Warning {
        if let Ok(msg) = pkt.data_as_string() {
            emit_warning(state, msg.to_string());
        } else {
            emit_warning(state, "Warning packet with invalid UTF-8 payload");
        }
        return;
    }

    if pkt.data_type() == DataType::FlightState {
        let current_state = { *state.state.lock().unwrap() };
        if current_state == groundstation_shared::FlightState::Startup {
            return;
        }
        if !state.all_boards_seen() {
            return;
        }
        let pkt_data = match pkt.data_as_u8() {
            Ok(data) => *data.first().expect("index 0 does not exist"),
            Err(_) => return,
        };
        let new_flight_state = match u8_to_flight_state(pkt_data) {
            Some(flight_state) => flight_state,
            None => return,
        };
        {
            let mut fs = state.state.lock().unwrap();
            *fs = new_flight_state;
        }
        let ts_ms = get_current_timestamp_ms() as i64;
        if let Err(e) = insert_with_retry(|| {
            sqlx::query("INSERT INTO flight_state (timestamp_ms, f_state) VALUES (?, ?)")
                .bind(ts_ms)
                .bind(pkt_data as i64)
                .execute(&state.db)
        })
            .await
        {
            eprintln!("DB insert into flight_state failed after retry: {e}");
        }

        let _ = state.state_tx.send(FlightStateMsg {
            state: new_flight_state,
        });
        return;
    }

    let ts_ms = pkt.timestamp() as i64;
    let data_type_str = pkt.data_type().as_str().to_string();

    let values = match pkt.data_as_f32() {
        Ok(v) => v,
        Err(_) => return,
    };
    let v0 = values.first().copied();
    let v1 = values.get(1).copied();
    let v2 = values.get(2).copied();
    let v3 = values.get(3).copied();
    let v4 = values.get(4).copied();
    let v5 = values.get(5).copied();
    let v6 = values.get(6).copied();
    let v7 = values.get(7).copied();

    if let Err(e) = insert_with_retry(|| {
        sqlx::query(
            "INSERT INTO telemetry (timestamp_ms, data_type, v0, v1, v2, v3, v4, v5, v6, v7) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
            .bind(ts_ms)
            .bind(&data_type_str)
            .bind(v0)
            .bind(v1)
            .bind(v2)
            .bind(v3)
            .bind(v4)
            .bind(v5)
            .bind(v6)
            .bind(v7)
            .execute(&state.db)
    })
        .await
    {
        eprintln!("DB insert into telemetry failed after retry: {e}");
    }

    let row = TelemetryRow {
        timestamp_ms: ts_ms,
        data_type: data_type_str,
        v0,
        v1,
        v2,
        v3,
        v4,
        v5,
        v6,
        v7,
    };

    let _ = state.ws_tx.send(row);
}

pub fn get_current_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration_since_epoch = now.duration_since(UNIX_EPOCH).unwrap();
    duration_since_epoch.as_millis() as u64
}
