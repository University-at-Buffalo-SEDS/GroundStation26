//! An SD-resolved recording for the contract fixtures
//! (`replay-bundle/sd.gsreplay` and the `sd` case of
//! `replay-bundle/exports/requests.json`).
//!
//! The recording is synthetic but has the tables and row shapes the SD
//! import and resolver write (`recording_sd_import.py`,
//! `recording_sd_resolve.py`): receipt rows in `telemetry`; one SD import
//! with its original file; SD samples; coverage segments; and the resolved
//! timeline `sd_replay_telemetry` with canonical-JSON provenance. It covers
//! SD rows that are one physical record (copied verbatim into the DAQ SD
//! layout), median-bin and paired (derived) rows, an over-range row, an
//! ambiguous boot's labelled sender, network rows kept in the resolved view
//! (one converted from the legacy raw scale) and network rows that follow
//! SD rows in time.
use super::{T0, binary_fixture, bundle_manifest, get_bytes, report_route, Fixtures};
use crate::state::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

pub(crate) const ID: &str = "groundstation_recording_2026-01-03_00-00-00_000.db";
const T2: i64 = T0 + 2 * 86_400_000;
const BOARD: &str = "DAQ-serial-fixture";
const BOOT: &str = "boot-fixture-1";

/// JSON as the resolver writes it (`canonical`: sorted keys, no spaces).
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let items: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical(&map[*k]))).collect();
            format!("{{{}}}", items.join(","))
        }
        Value::Array(items) => format!("[{}]", items.iter().map(canonical).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// A float as JSON writes it (`12.0`, not `12`).
fn f(x: f64) -> String {
    Value::from(x).to_string()
}

const DAQ_COLUMNS: [&str; 13] = [
    "timestamp_ms", "monotonic_ms", "sensor", "value", "raw_adc_code", "raw_value", "calibrated_value", "time_source",
    "adc_temperature_c", "adc_temperature_code", "uptime_ticks", "uptime_frequency_hz", "record_sequence",
];

struct Sample {
    id: i64,
    sensor: &'static str,
    fields: Value,
}

/// One SD record's thirteen cells (text, as the import stores them).
fn record(t: i64, sequence: i64, sensor: &'static str, raw: &str, calibrated: &str, over: bool) -> Value {
    let uptime = 5_000_000 + (t - T2) * 1_000;
    json!({
        "timestamp_ms": t.to_string(), "monotonic_ms": ((t - T2) + 5_000).to_string(), "sensor": sensor,
        "value": if over { "1" } else { "" }, "raw_adc_code": (1_000 + sequence * 17).to_string(),
        "raw_value": raw, "calibrated_value": calibrated, "time_source": "network",
        "adc_temperature_c": "24.5", "adc_temperature_code": "-200", "uptime_ticks": uptime.to_string(),
        "uptime_frequency_hz": "1000000", "record_sequence": sequence.to_string(),
    })
}

fn csv_line(fields: &Value) -> String {
    DAQ_COLUMNS.iter().map(|c| fields[*c].as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>().join(",")
}

/// Creates the SD-resolved recording `ID` under `root`.
pub(crate) async fn create(root: &Path) {
    let (db, _) = crate::telemetry_db::open_telemetry_db(&root.join(ID)).await.unwrap();
    // Receipt rows: what the station received.
    let mut receipt: Vec<(i64, Option<i64>, &str, &str, String)> = Vec::new();
    for i in 0..6i64 {
        let t = T2 + i * 1_000;
        let v = 0.25 + 0.125 * i as f64;
        receipt.push((t + 40, Some(t), "DAQ", "KG1000", format!("[{}]", f(v))));
        receipt.push((t + 40, Some(t), "DAQ", "LOADCELL_WEIGHT_KG", format!("[{}]", f(2.0 * v + 1.0))));
        receipt.push((t + 45, Some(t + 5), "DAQ", "KG50", format!("[{}]", f(9210.0 + 921.0 * i as f64))));
        receipt.push((t + 50, Some(t + 3), "VB", "OX_TANK_PRESSURE", format!("[{}]", f(500.5 + i as f64))));
        receipt.push((t + 52, Some(t + 4), "FC", "BAROMETER_DATA", format!("[{},{},25.5]", f(10.5 * i as f64), f(90000.0 - 12.5 * i as f64))));
        receipt.push((t + 60, Some(t + 2), "DAQ", "DAQ_POWER_MONITOR", format!("[1.25,0.5,{},{}]", f(12.0 + 0.25 * i as f64), f(0.5 + 0.125 * i as f64))));
        if i % 2 == 0 {
            receipt.push((t + 70, Some(t + 6), "VB", "VALVE_STATE", format!("[{},1,0,{},0,0,1]", (i / 2) % 2, i % 4 / 2)));
        }
    }
    receipt.sort_by_key(|r| r.0);
    let mut network_ids = Vec::new();
    for (received, source, sender, kind, values) in &receipt {
        let id = sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json) VALUES(?,?,?,?,?,'[]')")
            .bind(received).bind(source).bind(sender).bind(kind).bind(values).execute(&db).await.unwrap().last_insert_rowid();
        network_ids.push(id);
    }
    for statement in [
        "CREATE TABLE sd_imports(id INTEGER PRIMARY KEY, file_sha256 TEXT NOT NULL, filename TEXT NOT NULL, board_id TEXT NOT NULL, boot_id TEXT NOT NULL, stream_id TEXT NOT NULL, launch_session TEXT, schema_version TEXT NOT NULL, config_json TEXT NOT NULL, metadata_json TEXT NOT NULL, manifest_json TEXT NOT NULL, UNIQUE(file_sha256,board_id,boot_id,stream_id))",
        "CREATE TABLE sd_samples(id INTEGER PRIMARY KEY, identity_key TEXT NOT NULL, content_sha256 TEXT NOT NULL, sensor TEXT NOT NULL, data_type TEXT, timestamp_ms INTEGER NOT NULL, monotonic_ms INTEGER NOT NULL, time_source TEXT NOT NULL, uptime_ticks TEXT NOT NULL, uptime_frequency_hz INTEGER NOT NULL, record_sequence TEXT NOT NULL, fields_json TEXT NOT NULL, UNIQUE(identity_key,content_sha256))",
        "CREATE TABLE sd_sample_origins(import_id INTEGER NOT NULL, record_index INTEGER NOT NULL, sample_id INTEGER NOT NULL, PRIMARY KEY(import_id,record_index))",
        "CREATE TABLE sd_source_chunks(file_sha256 TEXT NOT NULL,offset INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(file_sha256,offset))",
        "CREATE TABLE sd_coverage_segments(id INTEGER PRIMARY KEY,import_id INTEGER,sender_id TEXT,data_type TEXT,start_ms INTEGER,end_ms INTEGER,board_id TEXT,boot_id TEXT,launch_session TEXT)",
        "CREATE TABLE sd_sequence_holes(import_id INTEGER,start_ms INTEGER,end_ms INTEGER)",
        "CREATE TABLE sd_replay_telemetry(id INTEGER PRIMARY KEY,timestamp_ms INTEGER NOT NULL,source_timestamp_ms INTEGER,data_type TEXT NOT NULL,sender_id TEXT,values_json TEXT,payload_json TEXT,provenance_json TEXT NOT NULL,over_range INTEGER)",
        "CREATE TABLE sd_replay_loadcell_units(id INTEGER PRIMARY KEY CHECK(id=1),unit TEXT NOT NULL)",
        "INSERT INTO sd_replay_loadcell_units VALUES(1,'adc_volts')",
        "CREATE TABLE sd_resolution_manifest(id INTEGER PRIMARY KEY CHECK(id=1),manifest_json TEXT NOT NULL)",
    ] {
        sqlx::query(statement).execute(&db).await.unwrap();
    }

    // The SD file: KG1000 records (raw volts and calibrated kg in one row)
    // and the supply pair, at the network clock; two records left unresolved.
    let mut samples: Vec<Sample> = Vec::new();
    let mut sequence = 0i64;
    for i in 0..6i64 {
        let t = T2 + i * 1_000 + 7;
        let v = 0.25 + 0.125 * i as f64;
        sequence += 1;
        samples.push(Sample { id: sequence, sensor: "kg1000_v", fields: record(t, sequence, "kg1000_v", &f(v), &f(2.0 * v + 1.0), i == 3) });
        sequence += 1;
        samples.push(Sample { id: sequence, sensor: "input_voltage_v", fields: record(t + 1, sequence, "input_voltage_v", "1.25", &f(12.0 + 0.25 * i as f64), false) });
        sequence += 1;
        samples.push(Sample { id: sequence, sensor: "input_current_a", fields: record(t + 1, sequence, "input_current_a", "0.5", &f(0.5 + 0.125 * i as f64), false) });
    }
    // A record the resolver could not place on one boot: a labelled series.
    sequence += 1;
    samples.push(Sample { id: sequence, sensor: "kg1000_v", fields: record(T2 + 2_500, sequence, "kg1000_v", "0.4375", "1.875", false) });
    for (offset, sensor) in [(8_500, "vdda_v"), (8_600, "excitation_v")] {
        sequence += 1;
        samples.push(Sample { id: sequence, sensor, fields: record(T2 + offset, sequence, sensor, "2.5", "2.5", false) });
    }
    let mut file = format!("{}\r\n# seds-sd,format=daq,version=4,board=DAQ,firmware=99db2e65c6310d6ed088831cde62ba683175eae6\r\n# calibration,mcp_vref_v=2.4\r\n", DAQ_COLUMNS.join(","));
    for sample in &samples {
        file.push_str(&csv_line(&sample.fields));
        file.push_str("\r\n");
    }
    let sha: String = ring::digest::digest(&ring::digest::SHA256, file.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let filename = "DAQ_0007.CSV";
    let sd_format = json!({"board": "DAQ", "firmware": "99db2e65c6310d6ed088831cde62ba683175eae6", "format": "daq", "mcp_vref_v": [2.4], "mcp_vref_v_default": false, "profile": "daq-v4", "version": 4});
    let manifest = json!({
        "board_id": BOARD, "boot_id": BOOT, "stream_id": "raw", "launch_session": "fixture-session",
        "selected_sensors": ["kg1000_v", "kg1000_kg", "input_supply"],
        "assignment": {"boot_and_session_verified": true, "sender_id": "DAQ", "start_utc_ms": T2 - 1_000, "end_utc_ms": T2 + 10_000, "max_gap_ms": 1_000, "units_axes_calibration_verified": true},
        "config": {},
    });
    let metadata = json!([{"record_index": 1, "fields": ["# seds-sd", "format=daq", "version=4", "board=DAQ", "firmware=99db2e65c6310d6ed088831cde62ba683175eae6"]}, {"record_index": 2, "fields": ["# calibration", "mcp_vref_v=2.4"]}]);
    sqlx::query("INSERT INTO sd_imports VALUES(1,?,?,?,?,'raw','fixture-session','seds-sd-csv-13-v1','{}',?,?)")
        .bind(&sha).bind(filename).bind(BOARD).bind(BOOT).bind(canonical(&metadata)).bind(canonical(&manifest)).execute(&db).await.unwrap();
    sqlx::query("INSERT INTO sd_source_chunks VALUES(?,0,?)").bind(&sha).bind(file.as_bytes().to_vec()).execute(&db).await.unwrap();
    for (index, sample) in samples.iter().enumerate() {
        let f = &sample.fields;
        let identity = format!("{BOARD}|{BOOT}|raw|{}|{}", sample.sensor, sample.id);
        sqlx::query("INSERT INTO sd_samples VALUES(?,?,?,?,NULL,?,?,'network',?,1000000,?,?)")
            .bind(sample.id).bind(&identity).bind(format!("{:064x}", sample.id)).bind(sample.sensor)
            .bind(f["timestamp_ms"].as_str().unwrap().parse::<i64>().unwrap()).bind(f["monotonic_ms"].as_str().unwrap().parse::<i64>().unwrap())
            .bind(f["uptime_ticks"].as_str().unwrap()).bind(f["record_sequence"].as_str().unwrap()).bind(canonical(f))
            .execute(&db).await.unwrap();
        sqlx::query("INSERT INTO sd_sample_origins VALUES(1,?,?)").bind(index as i64 + 3).bind(sample.id).execute(&db).await.unwrap();
    }
    for (id, kind, start, end) in [(1, "KG1000", T2 + 7, T2 + 5_007), (2, "LOADCELL_WEIGHT_KG", T2 + 7, T2 + 5_007), (3, "DAQ_POWER_MONITOR", T2 + 8, T2 + 5_008)] {
        sqlx::query("INSERT INTO sd_coverage_segments VALUES(?,1,'DAQ',?,?,?,?,?,'fixture-session')").bind(id).bind(kind).bind(start).bind(end).bind(BOARD).bind(BOOT).execute(&db).await.unwrap();
    }

    // The resolved timeline, ordered as the resolver numbers it.
    let sample_provenance = |sample: &Sample, segment: i64, mapping: Value| {
        let t: i64 = sample.fields["timestamp_ms"].as_str().unwrap().parse().unwrap();
        json!({
            "ambiguous": false, "board_id": BOARD, "boot_id": BOOT, "coverage_segment_id": segment, "data_type": null,
            "fields": sample.fields, "file_sha256": sha, "filename": filename,
            "identity_key": format!("{BOARD}|{BOOT}|raw|{}|{}", sample.sensor, sample.id), "import_id": 1,
            "launch_session": "fixture-session", "mapping": mapping, "origin": "sd", "received_timestamp_ms": null,
            "record_index_first": sample.id + 2, "record_index_last": sample.id + 2, "record_indices": [sample.id + 2],
            "record_indices_preview": false, "record_occurrences": 1, "sample_id": sample.id, "sd_sample_id": sample.id,
            "sensor": sample.sensor, "source_timestamp_ms": t, "stream_id": "raw", "utc_timestamp_ms": t,
        })
    };
    let mapping = |sensor: &str, field: &str, indices: Value, kind: &str, units: Value, extra: Value| {
        let mut m = json!({"data_type": kind, "field": field, "indices": indices, "profile": "daq-v4", "sd_format": sd_format, "sensor": sensor, "source_evidence": "99db2e65c6310d6ed088831cde62ba683175eae6", "units": units});
        for (k, v) in extra.as_object().into_iter().flatten() {
            m[k] = v.clone();
        }
        m
    };
    // (timestamp, sender, type, values, provenance, over_range)
    let mut rows: Vec<(i64, String, &str, String, Value, Option<i64>)> = Vec::new();
    for i in 0..6usize {
        let kg = &samples[i * 3];
        let t: i64 = kg.fields["timestamp_ms"].as_str().unwrap().parse().unwrap();
        let v = 0.25 + 0.125 * i as f64;
        let over = i == 3;
        let p = sample_provenance(kg, 1, mapping("kg1000_v", "raw_value", json!([0]), "KG1000", json!(["V"]),
            json!({"derived": "median_bin", "median": 3, "bin_us": 20000, "records": 1, "source_sensor": "kg1000_v", "first_sd_sample_id": kg.id, "last_sd_sample_id": kg.id})));
        rows.push((t, "DAQ".into(), "KG1000", if over { "[null]".into() } else { format!("[{}]", f(v)) }, p, if over { crate::over_range::pack(1, 0) } else { None }));
        let mut p = sample_provenance(kg, 2, mapping("kg1000_kg", "calibrated_value", json!([0]), "LOADCELL_WEIGHT_KG", json!(["kg"]), json!({})));
        if over {
            p["mapping"]["over_range"] = 1.into();
        }
        rows.push((t, "DAQ".into(), "LOADCELL_WEIGHT_KG", if over { "[null]".into() } else { format!("[{}]", f(2.0 * v + 1.0)) }, p, if over { crate::over_range::pack(1, 0) } else { None }));
        let (volts, amps) = (&samples[i * 3 + 1], &samples[i * 3 + 2]);
        let mut p = sample_provenance(volts, 3, mapping("input_supply", "null", json!([0, 1, 2, 3]), "DAQ_POWER_MONITOR", json!(["V", "V", "V", "A"]), json!({"derived": "pair", "paired_sd_sample_id": amps.id})));
        p["mapping"]["field"] = Value::Null;
        rows.push((t + 1, "DAQ".into(), "DAQ_POWER_MONITOR", format!("[1.25,0.5,{},{}]", f(12.0 + 0.25 * i as f64), f(0.5 + 0.125 * i as f64)), p, None));
    }
    // A second, unverified boot's record of the same channel: a labelled series.
    {
        let kg = &samples[18];
        let mut p = sample_provenance(kg, 2, mapping("kg1000_kg", "calibrated_value", json!([0]), "LOADCELL_WEIGHT_KG", json!(["kg"]), json!({})));
        p["ambiguous"] = true.into();
        rows.push((T2 + 2_500, format!("DAQ [SD {BOOT}]"), "LOADCELL_WEIGHT_KG", "[1.875]".into(), p, None));
    }
    // Network rows SD does not cover stay, on their source clock.
    for ((received, source, sender, kind, values), id) in receipt.iter().zip(&network_ids) {
        if matches!(*kind, "KG1000" | "LOADCELL_WEIGHT_KG" | "DAQ_POWER_MONITOR") {
            continue;
        }
        let mut p = json!({"ambiguous": false, "origin": "network", "original_network_id": id, "received_timestamp_ms": received, "source_timestamp_ms": source, "utc_timestamp_ms": source});
        let mut values = values.clone();
        if *kind == "KG50" {
            // Recorded on the legacy raw scale: the resolved view is in volts.
            let raw: f64 = values.trim_matches(['[', ']']).parse().unwrap();
            p["converted_from"] = json!({"units": "legacy_raw", "values_json": values, "volts_per_raw": "16000/921"});
            values = format!("[{}]", f(f64::from((f64::from(raw as f32) * (16000.0 / 921.0)) as f32)));
        }
        rows.push((source.unwrap(), (*sender).into(), kind, values, p, None));
    }
    let mut keyed: Vec<_> = rows.into_iter().enumerate().collect();
    keyed.sort_by(|(a_i, a), (b_i, b)| (a.0, &a.1, a.2, a_i).cmp(&(b.0, &b.1, b.2, b_i)));
    for (n, (_, (t, sender, kind, values, provenance, over))) in keyed.into_iter().enumerate() {
        sqlx::query("INSERT INTO sd_replay_telemetry VALUES(?,?,?,?,?,?,NULL,?,?)")
            .bind(n as i64 + 1).bind(t).bind(provenance["source_timestamp_ms"].as_i64()).bind(kind).bind(sender).bind(values).bind(canonical(&provenance)).bind(over)
            .execute(&db).await.unwrap();
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sd_replay_telemetry").fetch_one(&db).await.unwrap();
    let summary = json!({"axis": "canonical_utc", "coverage_segments": 3, "coverage_table": "sd_coverage_segments", "format": "seds-sd-resolution",
        "network_suppressed_in_resolved_view": 18, "original_network_preserved": true, "rows": rows, "sd_formats": [{"import_id": 1, "profile": "daq-v4"}],
        "unresolved_samples": 2, "version": 1});
    sqlx::query("INSERT INTO sd_resolution_manifest VALUES(1,?)").bind(canonical(&summary)).execute(&db).await.unwrap();
    // The calibration the station recorded: the report's KG1000 epoch.
    sqlx::query("CREATE TABLE IF NOT EXISTS calibration_history (timestamp_ms INTEGER NOT NULL, sender_id TEXT NOT NULL, sensor_id TEXT NOT NULL, config_json TEXT NOT NULL, PRIMARY KEY(timestamp_ms,sender_id,sensor_id))").execute(&db).await.unwrap();
    let calibration = json!({"calibration": {"ch1": {"m": 2.0, "b": 1.0}, "ch1_fit": null, "ch1_zero_raw": null, "points_ch1": [], "iadc": {"m": 1.0, "b": 0.0}}, "fill_targets": {"fill_source": "kg50", "target_kg": 8.0}, "flight_state": "NitrousFill"});
    sqlx::query("INSERT INTO calibration_history VALUES(?,'DAQ','KG1000',?)").bind(T2).bind(calibration.to_string()).execute(&db).await.unwrap();
    db.close().await;
}

/// The SD-resolved bundle (`replay-bundle/sd.gsreplay`, with the original
/// SD file) and the station's exports of the recording (case `sd`).
pub(super) async fn fixtures(root: &Path, state: &Arc<AppState>, app: &axum::Router, requests: &mut Vec<Value>, out: &mut Fixtures) {
    create(root).await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("Authorization", "Bearer viewer-token".parse().unwrap());
    let options = crate::recording_bundle::Options { records: Some(false), sd_files: true, sd_files_max_mb: Some(1), ..Default::default() };
    let bundle = crate::recording_bundle::build(state.clone(), &headers, ID, &options, T0).await.unwrap();
    let again = crate::recording_bundle::build(state.clone(), &headers, ID, &options, T0).await.unwrap();
    assert_eq!(bundle, again, "an SD-resolved bundle is reproducible");
    let manifest = bundle_manifest(&bundle);
    assert_eq!(manifest["sd"]["receipt_csv"]["included"], true, "{manifest}");
    assert_eq!(manifest["sd"]["files"]["included"], 1);
    let entry = |path: &str| super::zip_entry(&bundle, path).unwrap_or_else(|| panic!("{path} is bundled"));
    let (status, csv) = get_bytes(app, &format!("/api/recordings/{ID}/csv")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry("data/telemetry.csv"), csv, "the bundle's CSV is the resolved CSV");
    assert!(csv.starts_with(b"id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json,canonical_utc_ms,provenance_json\r\n"));
    out.add("replay-bundle/sd.manifest", manifest);
    binary_fixture("replay-bundle/sd.gsreplay", &bundle);

    let mut fetch = async |path: String, file: &str| {
        let (status, body) = get_bytes(app, &path).await;
        let mut entry = json!({"case": "sd", "path": path, "status": status.as_u16()});
        if status != StatusCode::OK {
            entry["body"] = String::from_utf8_lossy(&body).into();
        } else if let Some(name) = file.strip_suffix(".json") {
            out.add(&format!("replay-bundle/exports/{name}"), serde_json::from_slice(&body).unwrap());
            entry["file"] = format!("exports/{file}").into();
        } else {
            binary_fixture(&format!("replay-bundle/exports/{file}"), &body);
            entry["file"] = format!("exports/{file}").into();
        }
        requests.push(entry);
    };
    fetch(format!("/api/recordings/{ID}/csv"), "sd-recording.csv").await;
    fetch(format!("/api/recordings/csv?start_ms={T2}&end_ms={}", T2 + 10_000), "sd-range-full.csv").await;
    fetch(format!("/api/recordings/csv?start_ms={}&end_ms={}", T2 + 2_000, T2 + 4_000), "sd-range-window.csv").await;
    let all = Value::Null;
    let subset = json!(["[\"DAQ\",\"KG1000\",0]", "[\"DAQ\",\"LOADCELL_WEIGHT_KG\",0]", "[\"VB\",\"OX_TANK_PRESSURE\",0]", "[\"DAQ\",\"DAQ_POWER_MONITOR\",2]"]);
    let (w0, w1) = (T2 + 1_000, T2 + 4_000);
    for (request, file) in [
        (json!({"recording": ID, "format": "preview", "channels": all}), "sd-report-preview.json"),
        (json!({"recording": ID, "format": "preview", "channels": subset, "start_ms": w0, "end_ms": w1}), "sd-report-preview-window.json"),
        (json!({"recording": ID, "format": "csv", "channels": all}), "sd-report.csv"),
        (json!({"recording": ID, "format": "csv", "channels": subset, "start_ms": w0, "end_ms": w1}), "sd-report-window.csv"),
        (json!({"recording": ID, "format": "daq_csv", "channels": all}), "sd-report-daq.csv"),
        (json!({"recording": ID, "format": "daq_tsv", "channels": all}), "sd-report-daq.tsv"),
        (json!({"recording": ID, "format": "daq_csv", "channels": subset, "start_ms": w0, "end_ms": w1}), "sd-report-window-daq.csv"),
        // A physical SD record split across channels: refused.
        (json!({"recording": ID, "format": "daq_csv", "channels": ["[\"DAQ\",\"LOADCELL_WEIGHT_KG\",0]"]}), ""),
        (json!({"recording": ID, "format": "csv", "channels": null, "start_ms": T2 - 10_000, "end_ms": T2 - 5_000}), ""),
    ] {
        fetch(report_route(request), file).await;
    }
}
