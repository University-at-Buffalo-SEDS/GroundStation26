use crate::loadcell::LoadcellCalibrationFile;
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const TEST_FIRE_HEADER: u16 = 172;

#[derive(Default)]
struct ChamberSample {
    pressure: Option<Option<f32>>,
    voltage: Option<Option<f32>>,
    conflicting: bool,
}

impl ChamberSample {
    fn values(&self) -> (Option<f32>, Option<f32>) {
        if self.conflicting {
            (None, None)
        } else {
            (self.pressure.flatten(), self.voltage.flatten())
        }
    }
}

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
    let volts_from_id = crate::telemetry_db::loadcell_volts_from_id(&db).await;
    let rows = sqlx::query(
        r#"
        SELECT
            id,
            timestamp_ms,
            source_timestamp_ms AS original_source_timestamp_ms,
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
            'COMBUSTION_CHAMBER_PRESSURE',
            'COMBUSTION_CHAMBER_VOLTAGE',
            'DAQ_ADC_TEMPERATURE',
            'FUEL_TANK_PRESSURE',
            'IADC',
            'BATTERY_VOLTAGE',
            'PRESSURE_TRANSDUCER_CALIBRATED',
            'LOADCELL_WEIGHT_KG',
            'DAQ_POWER_MONITOR'
        )
        ORDER BY id ASC
        "#,
    )
    .fetch_all(&db)
    .await?;

    // Canonical PT rows and KG1000 share the original packet's two clocks and
    // payload. Join only that identity, never the previous pressure observation.
    // The raw combined row is not another load-cell sample.
    let mut chamber_samples: HashMap<_, ChamberSample> = HashMap::new();
    for row in &rows {
        let kind: &str = row.get("data_type");
        if !matches!(
            kind,
            "COMBUSTION_CHAMBER_PRESSURE" | "COMBUSTION_CHAMBER_VOLTAGE"
        ) || row.get::<&str, _>("sender_id") != "DAQ"
        {
            continue;
        }
        let value = parse_values_json(row.get::<Option<String>, _>("values_json").as_deref())
            .first()
            .copied()
            .flatten()
            .filter(|value| value.is_finite());
        let sample = chamber_samples.entry(chamber_packet_key(row)).or_default();
        let slot = if kind == "COMBUSTION_CHAMBER_PRESSURE" {
            &mut sample.pressure
        } else {
            &mut sample.voltage
        };
        match slot {
            Some(previous) if *previous != value => sample.conflicting = true,
            None => *slot = Some(value),
            _ => {}
        }
    }

    if let Some(parent) = csv_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut writer = std::io::BufWriter::new(std::fs::File::create(csv_path)?);
    writeln!(
        writer,
        "Rx_Timestamp,Header,Seq,Timestamp,1000kg ADC (V),Tank Pressure Raw,Battery Voltage,CRC,1000kg Calibrated,Weight,Thrust,Tank Pressure Calibrated,50kg ADC (V),50kg Calibrated,ADC Temperature C,Chamber Pressure (psig),Chamber Connector Voltage (V),DAQ Input Voltage (V),DAQ Input Current (A)"
    )?;
    // Load-cell coefficients are kg = f(ADC-pin volts): m in kg/V, b in kg.
    writeln!(
        writer,
        "CALIBRATION,,,,\"{} (kg/V)\",\"{}\",,,,,,,,,,,,",
        linear_formula(calibration.ch1.m, calibration.ch1.b),
        linear_formula(calibration.iadc.m, calibration.iadc.b)
    )?;

    let kg50 = crate::loadcell::kg50_daq_coefficients(calibration).map_err(anyhow::Error::msg)?;
    writeln!(
        writer,
        "CALIBRATION_50KG,,,,,,,,,,,,\"c0={};c1={};c2={};c3={};c4={};x0={};tare={} (x in V)\",,,,,,",
        kg50[0], kg50[1], kg50[2], kg50[3], kg50[4], kg50[5], kg50[6]
    )?;
    let mut state = ExportState::default();
    let mut seq: u16 = 0;
    let filters = crate::loadcell_zero::Service::default();
    let mut temperatures: Vec<crate::types::TelemetryRow> = Vec::new();
    for row in rows {
        let data_type: String = row.get("data_type");
        let mut values = parse_values_json(row.get::<Option<String>, _>("values_json").as_deref());
        if row.get::<i64, _>("id") < volts_from_id {
            // Recorded on the DAQ legacy raw scale: export volts throughout.
            crate::loadcell::legacy_loadcell_row_to_volts(
                &data_type,
                row.get::<Option<String>, _>("payload_json").as_deref(),
                &mut values,
            );
        }
        let first_value = values.first().copied().flatten();

        match data_type.as_str() {
            "DAQ_ADC_TEMPERATURE" => {
                let sender: String = row.get("sender_id");
                temperatures.retain(|r| r.sender_id != sender);
                temperatures.push(crate::types::TelemetryRow {
                    timestamp_ms: row.get("timestamp_ms"),
                    data_type,
                    sender_id: sender,
                    values,
                });
            }
            "FUEL_TANK_PRESSURE" | "IADC" => {
                state.pressure_raw = first_value;
            }
            "BATTERY_VOLTAGE" => {
                state.battery_voltage = first_value;
            }
            // The DAQ board supply ([VMON V, IMON V, input V, input A]) gets a
            // row of its own at its own source time, so supply dips plot
            // against the load-cell rows around them.
            "DAQ_POWER_MONITOR" if row.get::<&str, _>("sender_id") == "DAQ" => {
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let mut fields = vec![String::new(); 19];
                fields[0] = rx_timestamp;
                fields[3] = source_timestamp_ms.to_string();
                fields[17] = display_opt(values.get(2).copied().flatten());
                fields[18] = display_opt(values.get(3).copied().flatten());
                writeln!(writer, "{}", fields.join(","))?;
            }
            "PRESSURE_TRANSDUCER_CALIBRATED" => {
                state.pressure_calibrated = first_value;
            }
            "KG50" => {
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let temperature = crate::loadcell::recent_adc_temperature(
                    temperatures.iter(),
                    &row.get::<String, _>("sender_id"),
                    row.get("timestamp_ms"),
                );
                let sender: String = row.get("sender_id");
                let calibrated = first_value
                    .filter(|raw| !railed(calibration, &filters, &sender, "KG50", row.get("timestamp_ms"), *raw))
                    .and_then(|raw| {
                        crate::loadcell::temperature_corrected_raw(
                            calibration,
                            "KG50",
                            raw,
                            temperature,
                        )
                    })
                    .map(|raw| {
                        filters.filter(
                            calibration,
                            &row.get::<String, _>("sender_id"),
                            "KG50",
                            row.get("timestamp_ms"),
                            raw,
                        )
                    })
                    .and_then(|raw| {
                        crate::loadcell::calibrated_weight_kg(calibration, "KG50", raw)
                    });
                writeln!(
                    writer,
                    "{},{},{},{},,,,,,,,,{},{},{},,,,",
                    rx_timestamp,
                    TEST_FIRE_HEADER,
                    seq % 256,
                    source_timestamp_ms,
                    display_opt(first_value),
                    display_opt(calibrated),
                    display_opt(temperature)
                )?;
                seq = seq.wrapping_add(1);
            }
            "KG1000" => {
                let chamber = chamber_samples
                    .get(&chamber_packet_key(&row))
                    .map(ChamberSample::values)
                    .unwrap_or_default();
                let rx_timestamp: String = row.get("rx_timestamp");
                let source_timestamp_ms: i64 = row.get("source_timestamp_ms");
                let payload_json: String = row.get("payload_json");
                let raw_loadcell = first_value.unwrap_or_default();
                let temperature = crate::loadcell::recent_adc_temperature(
                    temperatures.iter(),
                    &row.get::<String, _>("sender_id"),
                    row.get("timestamp_ms"),
                );
                let sender: String = row.get("sender_id");
                let calibrated = Some(raw_loadcell)
                    .filter(|raw| !railed(calibration, &filters, &sender, "KG1000", row.get("timestamp_ms"), *raw))
                    .and_then(|raw| {
                        crate::loadcell::temperature_corrected_raw(calibration, "KG1000", raw, temperature)
                    })
                    .map(|raw| filters.filter(calibration, &sender, "KG1000", row.get("timestamp_ms"), raw))
                    .and_then(|raw| calibrated_loadcell(calibration, raw));
                let crc = crc16_ccitt_false(&parse_payload_json(&payload_json));
                writeln!(
                    writer,
                    "{},{},{},{},{},{},{},{},{},{},{},{},,,{},{},{},,",
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
                    display_opt(chamber.0),
                    display_opt(chamber.1),
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

fn chamber_packet_key(row: &sqlx::sqlite::SqliteRow) -> (String, i64, Option<i64>, String) {
    (
        row.get("sender_id"),
        row.get("timestamp_ms"),
        row.get("original_source_timestamp_ms"),
        row.get("payload_json"),
    )
}

/// A DAQ load-cell sample at an ADC or amplifier rail carries no load
/// information: the export keeps its volts and leaves its weight empty,
/// like the live LOADCELL_WEIGHT_KG, and resets the smoothing filter.
fn railed(
    calibration: &LoadcellCalibrationFile,
    filters: &crate::loadcell_zero::Service,
    sender: &str,
    sensor: &str,
    timestamp_ms: i64,
    volts: f32,
) -> bool {
    let railed = crate::loadcell::daq_rails_apply()
        && crate::loadcell::loadcell_volts_at_rail(calibration.kg50_input, sensor, volts);
    if railed {
        let _ = filters.filter(calibration, sender, sensor, timestamp_ms, f32::NAN);
    }
    railed
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

        crate::mode_suite!(TestFire);
        let dir = std::env::temp_dir().join(format!(
            "gs-kg50-csv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("recording.db");
        let csv_path = dir.join("recording.csv");
        let db = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY, timestamp_ms INTEGER, source_timestamp_ms INTEGER, data_type TEXT, values_json TEXT, payload_json TEXT, sender_id TEXT)")
            .execute(&db).await.unwrap();
        crate::telemetry_db::mark_loadcell_volts(&db).await.unwrap();
        // In-range DAQ volts (a value beyond the ADC rail exports no weight).
        for (i, raw) in [0.0625, 0.09375].iter().enumerate() {
            sqlx::query("INSERT INTO telemetry VALUES (?, ?, ?, 'KG50', ?, '[]', 'DAQ')")
                .bind(i as i64 + 1)
                .bind(10000_i64 + i as i64)
                .bind(9000_i64 + i as i64)
                .bind(format!("[{raw}]"))
                .execute(&db)
                .await
                .unwrap();
        }
        db.close().await;
        let mut cfg = LoadcellCalibrationFile::default();
        cfg.extra_channels.insert(
            "kg50".into(),
            crate::loadcell::GenericCalibrationChannel {
                linear: crate::loadcell::ChannelLinear {
                    m: Some(32.0),
                    b: Some(1.0),
                },
                ..Default::default()
            },
        );
        export_recording_csv(db_path.to_str().unwrap(), &csv_path, &cfg)
            .await
            .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let samples: Vec<Vec<&str>> = csv
            .lines()
            .skip(3)
            .map(|line| line.split(',').collect())
            .collect();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].len(), 19);
        assert_eq!(samples[0][15..], ["", "", "", ""]);
        assert_eq!(samples[1][15..], ["", "", "", ""]);
        assert_eq!(samples[0][3], "9000");
        assert_eq!(samples[0][12..14], ["0.0625", "3"]);
        assert_eq!(samples[0][14], ""); // No invented ADC temperature.
        assert_eq!(samples[1][3], "9001");
        assert_eq!(samples[1][12..14], ["0.09375", "4"]);
        assert_eq!(samples[0][4], ""); // No fabricated 1000 kg readings.
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// DAQ supply samples (DAQ_POWER_MONITOR) get their own rows at their own
    /// source time, between the load-cell rows, so a supply dip lines up with
    /// the KG1000/KG50 samples around it.
    #[tokio::test]
    async fn daq_supply_samples_export_between_load_cell_rows() {
        crate::mode_suite!(TestFire);
        let dir = std::env::temp_dir().join(format!(
            "gs-supply-csv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("recording.db");
        let csv_path = dir.join("recording.csv");
        let db = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY, timestamp_ms INTEGER, source_timestamp_ms INTEGER, data_type TEXT, values_json TEXT, payload_json TEXT, sender_id TEXT)")
            .execute(&db).await.unwrap();
        crate::telemetry_db::mark_loadcell_volts(&db).await.unwrap();
        for (id, kind, source, values, sender) in [
            (1, "KG1000", 9000, "[0.5]", "DAQ"),
            (2, "DAQ_POWER_MONITOR", 9002, "[1.29,0.24,14.2,0.5]", "DAQ"),
            (3, "KG1000", 9004, "[0.5]", "DAQ"),
            (4, "DAQ_POWER_MONITOR", 9006, "[1.01,0.24,11.1,null]", "DAQ"),
            (5, "DAQ_POWER_MONITOR", 9007, "[1,1,99,9]", "VB"),
        ] {
            sqlx::query("INSERT INTO telemetry VALUES (?, ?, ?, ?, ?, '[]', ?)")
                .bind(id)
                .bind(10000_i64 + id)
                .bind(source)
                .bind(kind)
                .bind(values)
                .bind(sender)
                .execute(&db)
                .await
                .unwrap();
        }
        db.close().await;
        export_recording_csv(db_path.to_str().unwrap(), &csv_path, &LoadcellCalibrationFile::default())
            .await
            .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let header: Vec<&str> = csv.lines().next().unwrap().split(',').collect();
        assert_eq!(header[17..], ["DAQ Input Voltage (V)", "DAQ Input Current (A)"]);
        let rows: Vec<Vec<&str>> = csv.lines().skip(1).map(|line| line.split(',').collect()).collect();
        // The two CALIBRATION rows quote formulas that contain commas, so only
        // the data rows split into exactly one field per column.
        let data = &rows[2..];
        assert!(data.iter().all(|row| row.len() == 19), "{csv}");
        assert_eq!(data.len(), 4, "one row per KG1000 and per DAQ supply sample");
        assert_eq!([data[0][3], data[0][4], data[0][17]], ["9000", "0.5", ""]);
        assert_eq!([data[1][1], data[1][3], data[1][4]], ["", "9002", ""]);
        assert_eq!(data[1][17..], ["14.2", "0.5"]);
        assert_eq!([data[2][3], data[2][4]], ["9004", "0.5"]);
        assert_eq!([data[3][3], data[3][17], data[3][18]], ["9006", "11.1", ""]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A load-cell sample at the ADC clip or the KG1000 amplifier floor (the
    /// 2026-11-16 test fire: code 0xB40000, 3.375 V, read as 1726 kg; legacy
    /// raw 0.0015, 0.026 V, as about -100 kg) keeps its volts and exports an
    /// empty weight.
    #[tokio::test]
    async fn railed_loadcell_samples_export_volts_without_a_weight() {
        crate::mode_suite!(TestFire);
        let dir = std::env::temp_dir().join(format!(
            "gs-railed-csv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("recording.db");
        let csv_path = dir.join("recording.csv");
        let db = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY, timestamp_ms INTEGER, source_timestamp_ms INTEGER, data_type TEXT, values_json TEXT, payload_json TEXT, sender_id TEXT)")
            .execute(&db).await.unwrap();
        crate::telemetry_db::mark_loadcell_volts(&db).await.unwrap();
        let rows = [
            ("KG1000", 0.5),
            ("KG1000", 3.375),
            ("KG1000", 0.026),
            ("KG1000", 0.5),
            ("KG50", 0.5),
            ("KG50", 3.375),
        ];
        for (i, (kind, raw)) in rows.iter().enumerate() {
            sqlx::query("INSERT INTO telemetry VALUES (?, ?, ?, ?, ?, '[]', 'DAQ')")
                .bind(i as i64 + 1)
                .bind(10000_i64 + i as i64)
                .bind(9000_i64 + i as i64)
                .bind(*kind)
                .bind(format!("[{raw}]"))
                .execute(&db)
                .await
                .unwrap();
        }
        db.close().await;
        let mut cfg = LoadcellCalibrationFile::default();
        cfg.ch1 = crate::loadcell::ChannelLinear { m: Some(125.0), b: Some(0.0) };
        cfg.extra_channels.insert(
            "kg50".into(),
            crate::loadcell::GenericCalibrationChannel {
                linear: crate::loadcell::ChannelLinear { m: Some(32.0), b: Some(1.0) },
                ..Default::default()
            },
        );
        export_recording_csv(db_path.to_str().unwrap(), &csv_path, &cfg)
            .await
            .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let samples: Vec<Vec<&str>> = csv
            .lines()
            .skip(3)
            .map(|line| line.split(',').collect())
            .collect();
        assert_eq!(samples.len(), 6);
        // KG1000: column 4 volts, 8 calibrated weight, 10 thrust.
        assert_eq!([samples[0][4], samples[0][8], samples[0][10]], ["0.5", "62.5", "62.5"]);
        assert_eq!([samples[1][4], samples[1][8], samples[1][10]], ["3.375", "", ""], "ADC clip");
        assert_eq!([samples[2][4], samples[2][8], samples[2][10]], ["0.026", "", ""], "amplifier floor");
        assert_eq!([samples[3][4], samples[3][8], samples[3][10]], ["0.5", "62.5", "62.5"]);
        // KG50: column 12 volts, 13 weight.
        assert_eq!(samples[4][12..14], ["0.5", "17"]);
        assert_eq!(samples[5][12..14], ["3.375", ""], "KG50 at the ADC clip");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_raw_recording_exports_volts_and_the_same_kg_after_migration() {
        crate::mode_suite!(TestFire);
        let dir = std::env::temp_dir().join(format!(
            "gs-legacy-csv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("recording.db");
        let csv_path = dir.join("recording.csv");
        let db = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", db_path.display()))
            .await
            .unwrap();
        // A pre-change recording: no load-cell unit marker, raw-scale values.
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY, timestamp_ms INTEGER, source_timestamp_ms INTEGER, data_type TEXT, values_json TEXT, payload_json TEXT, sender_id TEXT)")
            .execute(&db).await.unwrap();
        let raw = [0.0122373_f32, 0.020895265, 0.04];
        for (i, value) in raw.iter().enumerate() {
            sqlx::query("INSERT INTO telemetry VALUES (?, ?, ?, 'KG1000', ?, '[1]', 'DAQ')")
                .bind(i as i64 + 1)
                .bind(10000_i64 + i as i64)
                .bind(9000_i64 + i as i64)
                .bind(format!("[{value}]"))
                .execute(&db)
                .await
                .unwrap();
        }
        // P6 selections were volts already: tagged [1.0, volts] payload.
        sqlx::query("INSERT INTO telemetry VALUES (9, 10009, 9009, 'KG50', '[0.0042532505]', '[0,0,128,63,218,94,139,59]', 'DAQ')")
            .execute(&db).await.unwrap();
        db.close().await;
        let mut old: LoadcellCalibrationFile = serde_json::from_str(
            r#"{"version":1,"ch1":{"m":9481.619,"b":-115.9939},"ch1_zero_raw":0.0122373,
                "extra_channels":{"ch1":{"linear":{"m":9481.619,"b":-115.9939},"zero_raw":0.0122373,
                "fit":{"type":"linear"}}}}"#,
        )
        .unwrap();
        let before: Vec<f32> = raw
            .iter()
            .map(|r| crate::loadcell::calibrated_weight_kg(&old, "KG1000", *r).unwrap())
            .collect();
        crate::loadcell::migrate_calibration(&mut old);
        export_recording_csv(db_path.to_str().unwrap(), &csv_path, &old)
            .await
            .unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let rows: Vec<Vec<&str>> = csv.lines().skip(3).map(|l| l.split(',').collect()).collect();
        assert_eq!(rows.len(), 4);
        for (i, row) in rows[..3].iter().enumerate() {
            let volts: f32 = row[4].parse().unwrap();
            let expected = crate::loadcell::legacy_raw_to_volts(raw[i]);
            assert_eq!(volts, expected);
            let kg: f32 = row[8].parse().unwrap();
            assert!((kg - before[i]).abs() <= 1e-3 + before[i].abs() * 1e-5, "{kg} vs {}", before[i]);
        }
        assert_eq!(rows[3][12], "0.0042532505", "P6 volts are not rescaled");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
