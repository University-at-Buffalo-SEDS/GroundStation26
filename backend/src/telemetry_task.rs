use crate::state::AppState;
use groundstation_shared::TelemetryCommand;
use groundstation_shared::TelemetryRow;
use sedsprintf_rs_2026::config::DataType;

use crate::radio::RadioDevice;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

pub async fn telemetry_task(
    state: Arc<AppState>,
    router: Arc<sedsprintf_rs_2026::router::Router>,
    radio: Arc<Mutex<Box<dyn RadioDevice>>>,
    mut rx: mpsc::Receiver<TelemetryCommand>,
) {
    let mut radio_interval = interval(Duration::from_millis(1));
    let mut handle_interval = interval(Duration::from_millis(2));
    let mut router_interval = interval(Duration::from_millis(10));

    loop {
        tokio::select! {
                _ = radio_interval.tick() => {
                    match radio.lock().expect("failed to get lock").recv_packet(&*router){
                        Ok(_) => {
                            // Packet received and handled by router
                        }
                        Err(e) => {
                            println!("radio_task exited with error: {}", e);
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
                            println!("Arm command sent");

                        }
                        TelemetryCommand::Disarm => {
                            router.log_queue(
                                    DataType::MessageData,
                                    "Disarm".as_bytes()
                                ).expect("failed to log Arm command");
                            println!("Disarm command sent");
                        }
                        TelemetryCommand::Abort => {
                            router.log::<u8>(
                                    DataType::Abort,
                                    &[],
                                ).expect("failed to log Abort command");
                            println!("Abort command sent");
                        }
                    }
                }
                _ = handle_interval.tick() => {
                    handle_packet(&state).await;
                }
        }
    }
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

    let ts_ms = pkt.timestamp() as i64;
    let data_type_str = pkt.data_type().as_str().to_string();

    let values = match pkt.data_as_f32() {
        Ok(v) => v,
        Err(_) => return,
    };
    let v0 = values.get(0).copied();
    let v1 = values.get(1).copied();
    let v2 = values.get(2).copied();
    let v3 = values.get(3).copied();
    let v4 = values.get(4).copied();
    let v5 = values.get(5).copied();
    let v6 = values.get(6).copied();
    let v7 = values.get(7).copied();

    // Insert into DB
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
        .await
        .expect("DB insert into telemetry failed");

    // Build DTO to send to WebSocket listeners
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
