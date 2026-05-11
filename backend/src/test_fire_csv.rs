use crate::loadcell::LoadcellCalibrationFile;
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::io::Write as _;
use std::path::{Path, PathBuf};

const TEST_FIRE_HEADER: u16 = 172;

#[derive(Default)]
struct ExportState {
    pressure_raw: Option<f32>,
    battery_voltage: Option<f32>,
    pressure_calibrated: Option<f32>,
}

pub fn csv_path_for_db_path(db_path: &str) -> PathBuf {
    Path::new(db_path).with_extension("csv")
}

pub async fn export_recording_csv(
    db_path: &str,
    csv_path: &Path,
    calibration: &LoadcellCalibrationFile,
) -> Result<()> {
    let db = SqlitePool::connect(&format!("sqlite://{db_path}")).await?;
    let rows = sqlx::query(
        r#"
        SELECT
            id,
            timestamp_ms,
            COALESCE(source_timestamp_ms, timestamp_ms) AS source_timestamp_ms,
            strftime('%Y-%m-%dT%H:%M:%f', timestamp_ms / 1000.0, 'unixepoch') AS rx_timestamp,
            data_type,
            values_json,
            payload_json
        FROM telemetry
        WHERE data_type IN (
            'KG1000',
            'FUEL_TANK_PRESSURE',
            'IADC',
            'BATTERY_VOLTAGE',
            'PRESSURE_TRANSDUCER_CALIBRATED',
            'LOADCELL_WEIGHT_KG'
        )
        ORDER BY id ASC
        "#,
    )
    .fetch_all(&db)
    .await?;

    if let Some(parent) = csv_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut writer = std::io::BufWriter::new(std::fs::File::create(csv_path)?);
    writeln!(
        writer,
        "Rx_Timestamp,Header,Seq,Timestamp,1000kg Raw,Tank Pressure Raw,Battery Voltage,CRC,1000kg Calibrated,Weight,Thrust,Tank Pressure Calibrated"
    )?;
    writeln!(
        writer,
        "CALIBRATION,,,,\"{}\",\"{}\",,,,,,",
        linear_formula(calibration.ch1.m, calibration.ch1.b),
        linear_formula(calibration.iadc.m, calibration.iadc.b)
    )?;

    let mut state = ExportState::default();
    let mut seq: u16 = 0;
    for row in rows {
        let data_type: String = row.get("data_type");
        let values = parse_values_json(row.get::<Option<String>, _>("values_json").as_deref());
        let first_value = values.first().copied().flatten();

        match data_type.as_str() {
            "FUEL_TANK_PRESSURE" | "IADC" => {
                state.pressure_raw = first_value;
            }
            "BATTERY_VOLTAGE" => {
                state.battery_voltage = first_value;
            }
            "PRESSURE_TRANSDUCER_CALIBRATED" => {
                state.pressure_calibrated = first_value;
            }
            "KG1000" => {
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let payload_json: String = row.get("payload_json");
                let raw_loadcell = first_value.unwrap_or_default();
                let calibrated =
                    calibrated_loadcell(calibration, raw_loadcell).unwrap_or(raw_loadcell);
                let crc = crc16_ccitt_false(&parse_payload_json(&payload_json));
                writeln!(
                    writer,
                    "{},{},{},{},{},{},{},{},{},{},{},{}",
                    rx_timestamp,
                    TEST_FIRE_HEADER,
                    seq % 256,
                    source_timestamp_ms,
                    raw_loadcell,
                    display_opt(state.pressure_raw),
                    display_opt(state.battery_voltage),
                    crc,
                    calibrated,
                    0.0_f32,
                    calibrated,
                    display_opt(state.pressure_calibrated),
                )?;
                seq = seq.wrapping_add(1);
            }
            _ => {}
        }
    }

    writer.flush()?;
    db.close().await;
    Ok(())
}

fn calibrated_loadcell(calibration: &LoadcellCalibrationFile, raw_value: f32) -> Option<f32> {
    crate::loadcell::calibrated_sensor_value(
        calibration,
        crate::loadcell::RAW_LOADCELL_DATA_TYPE_1000KG,
        raw_value,
    )
}

fn linear_formula(m: Option<f32>, b: Option<f32>) -> String {
    format!("m={},b={}", m.unwrap_or(1.0), b.unwrap_or(0.0))
}

fn parse_values_json(raw: Option<&str>) -> Vec<Option<f32>> {
    raw.and_then(|json| serde_json::from_str::<Vec<Option<f32>>>(json).ok())
        .unwrap_or_default()
}

fn parse_payload_json(raw: &str) -> Vec<u8> {
    serde_json::from_str::<Vec<u8>>(raw).unwrap_or_default()
}

fn display_opt(value: Option<f32>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

fn crc16_ccitt_false(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFF_u16;
    for byte in bytes {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}
