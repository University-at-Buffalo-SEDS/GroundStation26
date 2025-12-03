use crate::state::AppState;
use crate::web::emit_warning;
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::Router;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

const ACCELERATION_X_MIN_THRESHOLD: f32 = -10.0; // m/s²
const ACCELERATION_X_MAX_THRESHOLD: f32 = 10.0; // m/s²

const ACCELERATION_Y_MIN_THRESHOLD: f32 = -10.0; // m/s²
const ACCELERATION_Y_MAX_THRESHOLD: f32 = 10.0; // m/s²
const ACCELERATION_Z_MIN_THRESHOLD: f32 = -10.0; // m/s²
const ACCELERATION_Z_MAX_THRESHOLD: f32 = 100.0; // m/s²

pub async fn safety_task(state: Arc<AppState>, router: Arc<Router>) {
    let mut abort = false;
    let mut count: u64 = 0;
    loop {
        sleep(Duration::from_millis(500)).await;

        // Snapshot current packets from the ring buffer
        let packets = {
            let rb = state.ring_buffer.lock().unwrap();
            let len = rb.len();

            if count >= 20 {
                emit_warning(
                    &state,
                    "Warning: No telemetry packets received for 10 seconds!",
                );
                println!("Safety: No telemetry packets received for 20 iterations!");
                count = 0;
            }

            if len == 0 {
                count += 1;
                Vec::new()
            } else {
                count = 0;
                // Most recent `len` packets, cloned so we can drop the lock
                rb.recent(len).into_iter().cloned().collect::<Vec<_>>()
            }
        };

        // Nothing to check
        if packets.is_empty() {
            continue;
        }

        // Loop through all recent packets and check safety conditions
        for pkt in packets {
            // Example safety check: if accel X > threshold, warn
            match pkt.data_type() {
                DataType::AccelData => {
                    let values = pkt.data_as_f32().unwrap_or_else(|_| vec![0f32; 3]);

                    // X axis: use `first()` and collapse the nested if
                    if let Some(accel_x) = values.first()
                        && ((ACCELERATION_X_MIN_THRESHOLD > *accel_x)
                            || (*accel_x > ACCELERATION_X_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration X threshold exceeded!");
                    }

                    // Y axis: collapse nested if
                    if let Some(accel_y) = values.get(1)
                        && ((ACCELERATION_Y_MIN_THRESHOLD > *accel_y)
                            || (*accel_y > ACCELERATION_Y_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration Y threshold exceeded!");
                    }

                    // Z axis: collapse nested if
                    if let Some(accel_z) = values.get(2)
                        && ((ACCELERATION_Z_MIN_THRESHOLD > *accel_z)
                            || (*accel_z > ACCELERATION_Z_MAX_THRESHOLD))
                    {
                        emit_warning(&state, "Critical: Acceleration Z threshold exceeded!");
                    }
                }
                DataType::GenericError => {
                    abort = true;
                    emit_warning(&state, "Generic Error received from vehicle!");
                    println!("Safety: Generic Error packet received");
                }
                _ => {}
            }
        }

        if abort {
            // Send abort command via router
            router
                .log::<u8>(
                    DataType::Abort,
                    "Safety Task Abort Command Issued".as_bytes(),
                )
                .expect("failed to log Abort command");
            println!("Safety task: Abort command sent");
            // Once aborted, we can exit the loop
            break;
        }
    }
}
