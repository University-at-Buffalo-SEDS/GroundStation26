use crate::state::AppState;
use sedsprintf_rs_2026::config::DataType;
use sedsprintf_rs_2026::router::Router;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

const ACCELERATION_X_MIN_THRESHOLD: f32 = -10.0; // m/s²
const ACCELERATION_X_MAX_THRESHOLD: f32 = 10.0; // m/s²

const ACCELERATION_Y_MIN_THRESHOLD: f32 = -10.0; // m/s²
const ACCELERATION_Y_MAX_THRESHOLD: f32 = 10.0; // m/s²
const ACCELERATION_Z_MIN_THRESHOLD: f32 = -2.0; // m/s²
const ACCELERATION_Z_MAX_THRESHOLD: f32 = 100.0; // m/s²

pub async fn safety_task(state: Arc<AppState>, router: Arc<Router>) {
    let mut abort = false;
    loop {
        sleep(Duration::from_millis(500)).await;

        // Snapshot current packets from the ring buffer
        let packets = {
            let rb = state.ring_buffer.lock().unwrap();
            let len = rb.len();

            if len == 0 {
                println!("Safety: no recent telemetry packets!");
                Vec::new()
            } else {
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
            if pkt.data_type() == DataType::AccelData {
                let values = pkt.data_as_f32();
                let values = values.unwrap_or_else(|_| {
                    vec![0f32; 3]
                });
                if let Some(accel_x) = values.get(0) {
                    if (ACCELERATION_X_MIN_THRESHOLD > *accel_x)
                        || (*accel_x > ACCELERATION_X_MAX_THRESHOLD)
                    {
                        abort = true;
                        println!("Safety: acceleration threshold exceeded (x = {})", accel_x);

                        // TODO: maybe insert a safety event into DB here too
                    }
                }
                if let Some(accel_y) = values.get(1) {
                    if (ACCELERATION_Y_MIN_THRESHOLD > *accel_y)
                        || (*accel_y > ACCELERATION_Y_MAX_THRESHOLD)
                    {
                        abort = true;
                        println!("Safety: acceleration threshold exceeded (y = {})", accel_y);

                        // TODO: maybe insert a safety event into DB here too
                    }
                }
                if let Some(accel_z) = values.get(2) {
                    if (ACCELERATION_Z_MIN_THRESHOLD > *accel_z)
                        || (*accel_z > ACCELERATION_Z_MAX_THRESHOLD)
                    {
                        abort = true;
                        println!("Safety: acceleration threshold exceeded (z = {})", accel_z);

                        // TODO: maybe insert a safety event into DB here too
                    }
                }
            }
        }

        if abort {
            // Send abort command via router
            router
                .log::<u8>(DataType::Abort, &[])
                .expect("failed to log Abort command");
            println!("Safety task: Abort command sent");
            // Once aborted, we can exit the loop
            break;
        }
    }
}
