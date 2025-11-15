use crate::state::AppState;
use crate::telemetry_decode::decode_f32_values;
use groundstation_shared::TelemetryCommand;
use groundstation_shared::TelemetryRow;
use sedsprintf_rs_2026::{
    config::{DataEndpoint, DataType},
    telemetry_packet::TelemetryPacket,
};

use rand;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

pub async fn telemetry_task(state: Arc<AppState>, mut cmd_rx: mpsc::Receiver<TelemetryCommand>) {
    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                // TODO: encode + send command over radio
                tracing::info!("TX command: {:?}", cmd);
            }

            _ = sleep(Duration::from_millis(100)) => {
                let value1:f32 = rand::random();
                let value2:f32 = rand::random();
                let value3:f32 = rand::random();

                // For now: synthesize a fake packet of some type, e.g. GyroData
                let gyro_pkt = TelemetryPacket::from_f32_slice(
                    DataType::GyroData,
                    &[value1, value2, value3],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                let value1:f32 = rand::random();
                let value2:f32 = rand::random();
                let value3:f32 = rand::random();
                let accel_pkt = TelemetryPacket::from_f32_slice(
                    DataType::AccelData,
                    &[value1, value2, value3],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                let value1:f32 = rand::random();
                let value2:f32 = rand::random();
                let gps_pkt = TelemetryPacket::from_f32_slice(
                    DataType::GpsData,
                    &[value1, value2],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                let value1:f32 = rand::random();
                let bat_volt_pkt = TelemetryPacket::from_f32_slice(
                    DataType::BatteryVoltage,
                    &[value1],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                let value1:f32 = rand::random();
                let bat_curr_pkt = TelemetryPacket::from_f32_slice(
                    DataType::BatteryCurrent,
                    &[value1],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                let value1:f32 = rand::random();
                let value2:f32 = rand::random();
                let value3:f32 = rand::random();
                let barr_pkt = TelemetryPacket::from_f32_slice(
                    DataType::BarometerData,
                    &[value1, value2, value3],                        // x, y, z
                    &[DataEndpoint::GroundStation],
                    get_current_timestamp_ms(),                                       // timestamp ms
                ).expect("failed to construct fake TelemetryPacket");

                handle_packet(&state, gyro_pkt).await;
                handle_packet(&state, accel_pkt).await;
                handle_packet(&state, gps_pkt).await;
                handle_packet(&state, bat_volt_pkt).await;
                handle_packet(&state, bat_curr_pkt).await;
                handle_packet(&state, barr_pkt).await;

            }
        }
    }
}

async fn handle_packet(state: &Arc<AppState>, pkt: TelemetryPacket) {
    // Keep raw packet in ring buffer if you still want it
    {
        let mut rb = state.ring_buffer.lock().unwrap();
        rb.push(pkt.clone());
    }

    let ts_ms = pkt.timestamp() as i64;
    let data_type_str = pkt.data_type().as_str().to_string();

    let values = decode_f32_values(&pkt).expect("died extracing data");
    let v0 = values.get(0).copied();
    let v1 = values.get(1).copied();
    let v2 = values.get(2).copied();

    // Insert into DB
    sqlx::query(
        "INSERT INTO telemetry (timestamp_ms, data_type, v0, v1, v2) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(ts_ms)
    .bind(&data_type_str)
    .bind(v0)
    .bind(v1)
    .bind(v2)
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
    };

    let _ = state.ws_tx.send(row);
}


fn get_current_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now();
    let duration_since_epoch = now.duration_since(UNIX_EPOCH).unwrap();
    duration_since_epoch.as_millis() as u64
}