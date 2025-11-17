use crate::state::AppState;
use sedsprintf_rs_2026::config::DataType;
use std::sync::Arc;
use sedsprintf_rs_2026::router::Router;
use tokio::time::{sleep, Duration};

pub async fn safety_task(state: Arc<AppState>, router: Arc<Router>) {
    let mut abort = false;
    loop {
        sleep(Duration::from_millis(500)).await;

        // Snapshot current packets from the ring buffer
        let packets = {
            let rb = state.ring_buffer.lock().unwrap();
            let len = rb.len();

            if len == 0 {
                tracing::warn!("Safety: no recent telemetry packets!");
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
                let values = crate::telemetry_decode::decode_f32_values(&pkt).unwrap_or_default();
                if let Some(accel_x) = values.get(0) {
                    if *accel_x > -10.0 {
                        tracing::warn!(
                            "Safety: acceleration threshold exceeded (x = {} m/s^2)",
                            accel_x
                        );
                        abort = true;
                        println!("Safety: acceleration threshold exceeded (x = {})", accel_x);

                        // TODO: maybe insert a safety event into DB here and start aborting
                    }
                }
            }
        }

        if abort {
            // Send abort command via router
            router
                .log(
                    DataType::MessageData,
                    "Abort".as_bytes(),
                )
                .expect("failed to log Abort command");
            println!("Safety task: Abort command sent");
            // Once aborted, we can exit the loop
            break;
        }
    }
}
