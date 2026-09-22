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
            sender_id,
            values_json,
            payload_json
        FROM telemetry
        WHERE data_type IN (
            'KG1000',
            'KG50',
            'DAQ_ADC_TEMPERATURE',
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
        "Rx_Timestamp,Header,Seq,Timestamp,1000kg Raw,Tank Pressure Raw,Battery Voltage,CRC,1000kg Calibrated,Weight,Thrust,Tank Pressure Calibrated,50kg Raw,50kg Calibrated,ADC Temperature C"
    )?;
    writeln!(
        writer,
        "CALIBRATION,,,,\"{}\",\"{}\",,,,,,,,",
        linear_formula(calibration.ch1.m, calibration.ch1.b),
        linear_formula(calibration.iadc.m, calibration.iadc.b)
    )?;

    let kg50 = crate::loadcell::kg50_daq_coefficients(calibration)
        .map_err(anyhow::Error::msg)?;
    writeln!(writer, "CALIBRATION_50KG,,,,,,,,,,,,\"c0={};c1={};c2={};c3={};c4={};x0={};tare={}\",",
        kg50[0], kg50[1], kg50[2], kg50[3], kg50[4], kg50[5], kg50[6])?;
    let mut state = ExportState::default();
    let mut seq: u16 = 0;
    let filters = crate::loadcell_zero::Service::default();
    let mut temperatures: Vec<crate::types::TelemetryRow> = Vec::new();
    for row in rows {
        let data_type: String = row.get("data_type");
        let values = parse_values_json(row.get::<Option<String>, _>("values_json").as_deref());
        let first_value = values.first().copied().flatten();

        match data_type.as_str() {
            "DAQ_ADC_TEMPERATURE" => {
                let sender: String = row.get("sender_id");
                temperatures.retain(|r| r.sender_id != sender);
                temperatures.push(crate::types::TelemetryRow { timestamp_ms: row.get("timestamp_ms"),
                    data_type, sender_id: sender, values });
            }
            "FUEL_TANK_PRESSURE" | "IADC" => {
                state.pressure_raw = first_value;
            }
            "BATTERY_VOLTAGE" => {
                state.battery_voltage = first_value;
            }
            "PRESSURE_TRANSDUCER_CALIBRATED" => {
                state.pressure_calibrated = first_value;
            }
            "KG50" => {
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let temperature = crate::loadcell::recent_adc_temperature(temperatures.iter(),
                    &row.get::<String, _>("sender_id"), row.get("timestamp_ms"));
                let calibrated = first_value.and_then(|raw| crate::loadcell::temperature_corrected_raw(calibration, "KG50", raw, temperature))
                    .map(|raw| filters.filter(calibration, &row.get::<String, _>("sender_id"), "KG50", row.get("timestamp_ms"), raw))
                    .and_then(|raw| crate::loadcell::calibrated_weight_kg(calibration, "KG50", raw));
                writeln!(writer, "{},{},{},{},,,,,,,,,{},{},{}",
                    rx_timestamp, TEST_FIRE_HEADER, seq % 256, source_timestamp_ms,
                    display_opt(first_value), display_opt(calibrated), display_opt(temperature))?;
                seq = seq.wrapping_add(1);
            }
            "KG1000" => {
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let payload_json: String = row.get("payload_json");
                let raw_loadcell = first_value.unwrap_or_default();
                let temperature = crate::loadcell::recent_adc_temperature(temperatures.iter(),
                    &row.get::<String, _>("sender_id"), row.get("timestamp_ms"));
                let calibrated = crate::loadcell::temperature_corrected_raw(calibration, "KG1000", raw_loadcell, temperature)
                    .map(|raw| filters.filter(calibration, &row.get::<String, _>("sender_id"), "KG1000", row.get("timestamp_ms"), raw))
                    .and_then(|raw| calibrated_loadcell(calibration, raw));
                let crc = crc16_ccitt_false(&parse_payload_json(&payload_json));
                writeln!(
                    writer,
                    "{},{},{},{},{},{},{},{},{},{},{},{},,,{}",
                    rx_timestamp,
                    TEST_FIRE_HEADER,
                    seq % 256,
                    source_timestamp_ms,
                    raw_loadcell,
                    display_opt(state.pressure_raw),
                    display_opt(state.battery_voltage),
                    crc,
                    display_opt(calibrated),
                    0.0_f32,
                    display_opt(calibrated),
                    display_opt(state.pressure_calibrated),
                    display_opt(temperature),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kg50_only_recording_exports_every_sample_with_its_timestamp() {
        let dir = std::env::temp_dir().join(format!("gs-kg50-csv-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("recording.db");
        let csv_path = dir.join("recording.csv");
        let db = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display())).await.unwrap();
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY, timestamp_ms INTEGER, source_timestamp_ms INTEGER, data_type TEXT, values_json TEXT, payload_json TEXT, sender_id TEXT)")
            .execute(&db).await.unwrap();
        for (i, raw) in [2.0, 3.0].iter().enumerate() {
            sqlx::query("INSERT INTO telemetry VALUES (?, ?, ?, 'KG50', ?, '[]', 'DAQ')")
                .bind(i as i64).bind(10000_i64 + i as i64).bind(9000_i64 + i as i64)
                .bind(format!("[{raw}]")).execute(&db).await.unwrap();
        }
        db.close().await;
        let mut cfg = LoadcellCalibrationFile::default();
        cfg.extra_channels.insert("kg50".into(), crate::loadcell::GenericCalibrationChannel {
            linear: crate::loadcell::ChannelLinear { m: Some(2.0), b: Some(1.0) },
            ..Default::default()
        });
        export_recording_csv(db_path.to_str().unwrap(), &csv_path, &cfg).await.unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let samples: Vec<Vec<&str>> = csv.lines().skip(3).map(|line| line.split(',').collect()).collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].len(), 14);
        assert_eq!(samples[0][3], "9000");
        assert_eq!(samples[0][12..], ["2", "5"]);
        assert_eq!(samples[1][3], "9001");
        assert_eq!(samples[1][12..], ["3", "7"]);
        assert_eq!(samples[0][4], ""); // No fabricated 1000 kg readings.
        std::fs::remove_dir_all(dir).unwrap();
    }
}
