//! Replay from every saved file kind, in every recorded station mode, without
//! touching the live station.
use super::sources::{self, Source};
use crate::recording_export::tests::{TestDirectory, export_state};
use crate::recording_store::{
    self, CAPTURE_FILE, Capture, EventKind, MANIFEST_FILE, SEGMENT_PREFIX, materialize, rolling_dir, stamp,
};
use crate::runtime_mode::test_support::thread_mode;
use crate::station_mode::Mode;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

const T0: i64 = 1_800_000_000_000;

/// One saved telemetry row: (timestamp, sender, data type, values, payload).
type Row<'a> = (i64, &'a str, &'a str, Option<&'a str>, Option<&'a str>);

async fn write_db(path: &Path, rows: &[Row<'_>]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let (db, _) = crate::telemetry_db::open_telemetry_db(path).await.unwrap();
    for (ts, sender, ty, values, payload) in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,?,?,?,?)")
            .bind(ts)
            .bind(ts)
            .bind(ty)
            .bind(sender)
            .bind(values)
            .bind(payload)
            .execute(&db)
            .await
            .unwrap();
    }
    crate::telemetry_db::close_and_finalize_sqlite(db, &path.to_string_lossy()).await;
}

async fn write_segment(root: &Path, start_ms: i64, rows: &[Row<'_>]) -> String {
    let dir = rolling_dir(root);
    let stem = format!("{SEGMENT_PREFIX}{}", stamp(start_ms));
    write_db(&dir.join(format!("{stem}.db")), rows).await;
    recording_store::RecordingId::segment_id(&stem)
}

fn mode_row(mode: &str, provenance: &str, igt: bool, hitl_replay: bool) -> String {
    json!({"mode": mode, "igt_active": igt, "mock_wet_dress": false, "hitl_replay": hitl_replay,
           "provenance": provenance, "firmware_sim": provenance == "firmware_sim"})
    .to_string()
}

fn capture(kind: EventKind, mode: &str, provenance: &str, start: i64, end: i64) -> Capture {
    Capture {
        serial: 1,
        kind,
        key: "k".into(),
        label: format!("{kind:?} replay fixture"),
        event_start_ms: start,
        window_start_ms: start - 10_000,
        event_end_ms: Some(end),
        post_roll_ms: 5_000,
        max_end_ms: end + 3_600_000,
        result: Some("Done".into()),
        operator: None,
        station_mode: mode.into(),
        provenance: Some(provenance.into()),
        details: Value::Null,
        materializing: false,
    }
}

struct Harness {
    _root: TestDirectory,
    root: PathBuf,
    app: Router,
}

async fn harness() -> Harness {
    sources::clear_cache().await;
    let root = TestDirectory::new();
    let path = root.0.clone();
    let state = export_state(&path, true).await;
    let app = crate::recording_export::routes().with_state(state.clone());
    Harness { _root: root, root: path, app }
}

impl Harness {
    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let response = self.app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }
    async fn ok(&self, uri: &str) -> Value {
        let (status, value) = self.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {value}");
        value
    }
    async fn context(&self, id: &str) -> Value {
        self.ok(&format!("/api/recordings/{}/replay/context", enc(id))).await
    }
    async fn layout(&self, id: &str, view: Option<&str>) -> Value {
        let q = view.map(|v| format!("?view={v}")).unwrap_or_default();
        self.ok(&format!("/api/recordings/{}/replay/layout{q}", enc(id))).await
    }
    /// Every row of `types` in the replay, through the bounded window API.
    async fn rows(&self, id: &str, types: &[&str]) -> Vec<Value> {
        let snap = self.ok(&format!("/api/recordings/{}/replay", enc(id))).await;
        let (Some(start), Some(end)) = (snap["start_ms"].as_i64(), snap["end_ms"].as_i64()) else {
            return Vec::new();
        };
        let upper = snap["upper_id"].as_i64().unwrap();
        let types = enc(&serde_json::to_string(types).unwrap());
        let mut out = Vec::new();
        let mut from = start;
        while from <= end {
            let to = (from + 29_999).min(end);
            let mut after = 0;
            loop {
                let page = self
                    .ok(&format!("/api/recordings/{}/replay/window?upper_id={upper}&after_id={after}&start_ms={from}&end_ms={to}&types={types}", enc(id)))
                    .await;
                out.extend(page["rows"].as_array().unwrap().iter().cloned());
                after = page["next_id"].as_i64().unwrap();
                if !page["has_more"].as_bool().unwrap() {
                    break;
                }
            }
            from = to + 1;
        }
        out
    }
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Rows every mode fixture carries, plus each mode's own.
fn mode_rows<'a>(mode_payload: &'a str, mode: &str) -> Vec<Row<'a>> {
    let mut rows: Vec<Row<'a>> = vec![
        (T0, "GS", "GS_STATION_MODE", None, Some(mode_payload)),
        (T0 + 1_000, "VB", "PRESSURE_TRANSDUCER_CALIBRATED", Some("[512.5]"), None),
        (T0 + 2_000, "DAQ", "LOADCELL_50_WEIGHT_KG", Some("[3.25]"), None),
    ];
    match mode {
        "test_fire" => rows.extend([
            (T0 + 3_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[812.0]"), None),
            (T0 + 3_100, "DAQ", "COMBUSTION_CHAMBER_PRESSURE", Some("[402.0]"), None),
        ]),
        "integrated_ground_test" => rows.extend([
            (T0 + 500, "GS", "GS_IGT", None, Some(r#"{"fc_mode":"sim"}"#)),
            (T0 + 3_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[0.4]"), None),
            (T0 + 3_100, "FC", "SIM_SAMPLE", Some("[1200.0, 85.0, 1.0]"), None),
            (T0 + 3_200, "FC", "BAROMETER_DATA", Some("[0.5, 98765.0, 21.5]"), None),
            (T0 + 3_300, "FC", "GPS_DATA", Some("[45.0, -73.0, 120.0]"), None),
            (T0 + 3_400, "FC", "FLIGHT_STATE", Some("[8]"), None),
        ]),
        _ => rows.extend([
            (T0 + 3_000, "FC", "BAROMETER_DATA", Some("[150.0, 98000.0, 20.0]"), None),
            (T0 + 3_100, "FC", "FLIGHT_STATE", Some("[8]"), None),
        ]),
    }
    rows
}

/// A round trip per recorded mode, from the rolling segment and from the
/// event capture made of it: the replay presents the recorded mode's view,
/// tabs, bodies and banners while the station stays in normal mode.
#[tokio::test]
async fn every_mode_replays_in_its_own_view_from_segments_and_captures() {
    let _live = thread_mode(Mode::Normal);
    // (recorded mode, provenance, igt, hitl replay, expected default view)
    let cases = [
        ("normal", "hardware", false, false, "normal"),
        ("test_fire", "hardware", false, false, "test_fire"),
        ("testing", "hardware", false, false, "testing"),
        ("hitl", "hardware", false, false, "hitl"),
        ("integrated_ground_test", "hardware", true, false, "integrated_ground_test"),
        ("normal", "firmware_sim", false, false, "normal"),
        ("test_fire", "firmware_sim", false, false, "test_fire"),
        ("integrated_ground_test", "firmware_sim", true, false, "integrated_ground_test"),
    ];
    for (mode, provenance, igt, hitl_replay, view) in cases {
        let h = harness().await;
        let payload = mode_row(mode, provenance, igt, hitl_replay);
        let rows = mode_rows(&payload, mode);
        let segment = write_segment(&h.root, T0 - 1_000, &rows).await;
        let saved = materialize(&h.root, &capture(EventKind::Manual, mode, provenance, T0 + 2_000, T0 + 3_000), T0 + 20_000)
            .await
            .unwrap();
        for id in [segment.as_str(), saved.id.as_str()] {
            let ctx = h.context(id).await;
            let label = format!("{mode}/{provenance} {id}");
            assert_eq!(ctx["replay"], true, "{label}");
            assert_eq!(ctx["read_only"], true, "{label}");
            assert_eq!(ctx["recorded_mode"], mode, "{label}");
            assert_eq!(ctx["mode_source"], "recording_rows", "{label}");
            assert_eq!(ctx["default_view"], view, "{label}");
            assert_eq!(ctx["provenance"], provenance, "{label}");
            assert_eq!(ctx["firmware_sim"], provenance == "firmware_sim", "{label}");
            assert_eq!(ctx["simulated"], provenance != "hardware" || mode == "hitl", "{label}");
            assert_eq!(ctx["igt"], igt, "{label}");
            assert_eq!(ctx["test_fire"], mode == "test_fire", "{label}");
            assert_eq!(ctx["live_station_mode"], "normal", "{label}");
            let tabs: Vec<&str> = ctx["tabs"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
            match mode {
                "test_fire" => {
                    assert!(tabs.contains(&"gse") && tabs.contains(&"thrust") && !tabs.contains(&"map"), "{label} {tabs:?}");
                }
                "integrated_ground_test" => {
                    assert!(tabs.contains(&"stand") && tabs.contains(&"rocket") && tabs.contains(&"estimator"), "{label}");
                    let bodies = ctx["bodies"].as_array().unwrap();
                    assert_eq!(bodies[0]["label"], "Stand (real)");
                    assert_eq!(bodies[1]["label"], "Rocket flight (replayed)");
                    assert_eq!(bodies[1]["simulated_types"], json!(["SIM_SAMPLE"]));
                    assert!(bodies[0]["senders"].as_array().unwrap().contains(&json!("DAQ")));
                    assert!(bodies[1]["senders"].as_array().unwrap().contains(&json!("FC")));
                    assert_eq!(ctx["sim_sample_rows"], 1, "{label}");
                }
                _ => assert!(tabs.contains(&"map") && tabs.contains(&"estimator") && tabs.contains(&"recovery"), "{label}"),
            }
            if mode != "integrated_ground_test" {
                assert!(ctx["bodies"].as_array().unwrap().is_empty(), "{label}");
            }
            if id == saved.id {
                assert_eq!(ctx["source_kind"], "capture");
                assert_eq!(ctx["manifest"]["station_mode"], mode);
            }
            // The recorded mode's layout, read only.
            let layout = h.layout(id, None).await;
            assert_eq!(layout["replay"]["view"], view, "{label}");
            assert_eq!(layout["replay"]["read_only"], true);
            assert!(layout["actions_tab"]["actions"].as_array().unwrap().is_empty(), "replay layouts carry no commands");
            assert_eq!(layout["test_fire_mode"], view == "test_fire", "{label}");
            assert_eq!(layout["igt_mode"], view == "integrated_ground_test", "{label}");
            // The recorded rows replay, including the replayed (SIM_SAMPLE) flight.
            let rows = h.rows(id, &["SIM_SAMPLE", "LOADCELL_50_WEIGHT_KG", "BAROMETER_DATA"]).await;
            assert!(rows.iter().any(|r| r["data_type"] == "LOADCELL_50_WEIGHT_KG"), "{label}");
            assert_eq!(rows.iter().any(|r| r["data_type"] == "SIM_SAMPLE"), mode == "integrated_ground_test", "{label}");
        }
        assert_eq!(crate::runtime_mode::current(), Mode::Normal, "replay never changes the live mode");
    }
}

/// HITL simulates any configuration: the view picker offers every mode's
/// layout, defaults to the recorded HITL configuration (the ground-test
/// replay opens in the IGT view) and never changes the live station mode.
#[tokio::test]
async fn hitl_replay_offers_every_view_and_defaults_to_its_configuration() {
    let _live = thread_mode(Mode::TestFire);
    let h = harness().await;
    let plain = mode_row("hitl", "hardware", false, false);
    let plain_id = write_segment(&h.root, T0, &mode_rows(&plain, "hitl")).await;
    let ctx = h.context(&plain_id).await;
    assert_eq!((ctx["hitl"].as_bool(), ctx["default_view"].as_str()), (Some(true), Some("hitl")));
    assert_eq!(
        ctx["view_options"],
        json!(["normal", "test_fire", "testing", "hitl", "integrated_ground_test"])
    );
    for view in ["normal", "test_fire", "testing", "hitl", "integrated_ground_test"] {
        let layout = h.layout(&plain_id, Some(view)).await;
        assert_eq!(layout["replay"]["view"], view);
        assert_eq!(layout["test_fire_mode"], view == "test_fire");
        assert_eq!(layout["igt_mode"], view == "integrated_ground_test");
        assert!(layout["actions_tab"]["actions"].as_array().unwrap().is_empty());
    }
    assert_eq!(h.get(&format!("/api/recordings/{plain_id}/replay/layout?view=launch")).await.0, StatusCode::BAD_REQUEST);

    let with_replay = mode_row("hitl", "hardware", true, true);
    let replay_id = write_segment(&h.root, T0 + 3_600_000, &mode_rows(&with_replay, "integrated_ground_test")).await;
    let ctx = h.context(&replay_id).await;
    assert_eq!(ctx["hitl_replay"], true);
    assert_eq!(ctx["igt"], true);
    assert_eq!(ctx["default_view"], "integrated_ground_test");
    assert_eq!(h.layout(&replay_id, None).await["replay"]["view"], "integrated_ground_test");
    assert_eq!(crate::runtime_mode::current(), Mode::TestFire, "the picker never changes the live mode");
}

/// A range of the rolling store spanning several segments replays as one
/// recording, keeps every row once and starts from the mode in force.
#[tokio::test]
async fn rolling_range_spans_segments_with_the_mode_in_force() {
    let _live = thread_mode(Mode::Normal);
    let h = harness().await;
    let tf = mode_row("test_fire", "hardware", false, false);
    write_segment(&h.root, T0, &[(T0, "GS", "GS_STATION_MODE", None, Some(&tf)), (T0 + 1_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[1.0]"), None), (T0 + 50_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[2.0]"), None)]).await;
    write_segment(&h.root, T0 + 60_000, &[(T0 + 60_000, "GS", "GS_STATION_MODE", None, Some(&tf)), (T0 + 61_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[3.0]"), None), (T0 + 70_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[4.0]"), None)]).await;
    write_segment(&h.root, T0 + 120_000, &[(T0 + 120_000, "GS", "GS_STATION_MODE", None, Some(&tf)), (T0 + 125_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[5.0]"), None)]).await;
    let id = format!("range.{}.{}", T0 + 10_000, T0 + 122_000);
    let ctx = h.context(&id).await;
    assert_eq!(ctx["source_kind"], "range");
    assert_eq!(ctx["recorded_mode"], "test_fire", "context row from before the range start");
    let values: Vec<f64> = h.rows(&id, &["LOADCELL_WEIGHT_KG"]).await.iter().map(|r| r["values"][0].as_f64().unwrap()).collect();
    assert_eq!(values, vec![2.0, 3.0, 4.0], "rows of the range from three segments, each once");
    // Malformed and oversized ranges are refused; a range with no segments is not found.
    assert_eq!(h.get(&format!("/api/recordings/range.{}.{}/replay", T0, T0)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(h.get(&format!("/api/recordings/range.{}.{}/replay", T0, T0 + sources::MAX_RANGE_MS + 1)).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(h.get("/api/recordings/range.5.10/replay").await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.get("/api/recordings/range.x.10/replay").await.0, StatusCode::BAD_REQUEST);
}

const BOARD_HEADER: &str = "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence";

fn attach_board_file(folder: &Path, board: &str, name: &str, lines: &[String], lead_up: Option<(&str, &[String])>) {
    let dir = folder.join("board_sd").join(board);
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, lines: &[String]| {
        std::fs::write(dir.join(name), format!("{BOARD_HEADER}\n# calibration,ch0,1,0\n{}\n", lines.join("\n"))).unwrap();
    };
    write(name, lines);
    if let Some((lead, lines)) = lead_up {
        write(lead, lines);
    }
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(folder.join(MANIFEST_FILE)).unwrap()).unwrap();
    manifest["board_sd"].as_array_mut().unwrap().push(json!({
        "board": board, "file": name, "bytes": 1, "sha256": "00", "data_from_ms": T0, "data_to_ms": T0 + 10_000,
        "rows": lines.len(), "matched_by": "utc_window", "source": "network_download",
        "lead_up_file": lead_up.map(|(n, _)| n), "lead_up_from_ms": null, "lead_up_rows": lead_up.map_or(0, |(_, l)| l.len()),
    }));
    std::fs::write(folder.join(MANIFEST_FILE), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

/// A test-fire capture merged with its DAQ SD launch file and lead-up on
/// network time; the SD rows keep a sender of their own. The SD files replay
/// alone too, and the burn report's thrust curve is the overlay.
#[tokio::test]
async fn capture_merges_board_sd_on_network_time_and_overlays_the_burn_report() {
    let _live = thread_mode(Mode::Normal);
    let h = harness().await;
    let tf = mode_row("test_fire", "hardware", false, false);
    write_segment(&h.root, T0 - 30_000, &[(T0 - 30_000, "GS", "GS_STATION_MODE", None, Some(&tf)), (T0 + 1_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[10.0]"), None), (T0 + 2_000, "DAQ", "LOADCELL_WEIGHT_KG", Some("[400.0]"), None)]).await;
    let saved = materialize(&h.root, &capture(EventKind::Launch, "test_fire", "hardware", T0, T0 + 3_000), T0 + 30_000).await.unwrap();
    {
        let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", saved.folder.join(CAPTURE_FILE).display())).await.unwrap();
        sqlx::query("INSERT INTO alerts(timestamp_ms,severity,message) VALUES(?,'critical','Chamber pressure high')").bind(T0 + 1_200).execute(&db).await.unwrap();
        db.close().await;
    }
    let daq = |t: i64, sensor: &str, v: f64, clock: &str| format!("{t},{},{sensor},{v},,,,{clock},,,{},1000000,{}", t - T0 + 5_000, (t - T0 + 5_000) * 1000, t - T0);
    let launch = vec![
        daq(T0 + 1_000, "kg1000_network", 11.0, "network"),
        daq(T0 + 1_001, "kg1000_network", 12.0, "network"),
        daq(T0 + 1_500, "chamber_pressure_raw_volts_psi", 350.0, "network"),
        daq(T0 + 1_600, "board_temperature", 31.5, "network"),
        daq(5, "kg1000_network", 99.0, "local"),
    ];
    let lead = vec![daq(T0 - 2_000, "kg1000_network", 0.5, "network")];
    attach_board_file(&saved.folder, "DAQ", "DAQ_LAUNCH_0001.CSV", &launch, Some(("DAQ_LAUNCH_0001.CSV.leadup.csv", &lead)));
    // The station's saved burn report beside the capture.
    std::fs::write(saved.folder.join(crate::burn_report::REPORT_FILE), json!({
        "id": "launch.x", "capture_id": saved.id, "capture_label": "x", "status": "final", "simulated": false,
        "mode": "test_fire", "created_ms": T0, "ignition_ms": T0 + 1_000, "ignition_source": "main_valve",
        "total_impulse_ns": 1000.0, "burn_time_s": 1.0, "peak_thrust_n": 2000.0, "average_thrust_n": 1000.0,
        "impulse_sigma_ns": 10.0, "flags": [], "curve": [[0.0, 0.0], [0.5, 2000.0], [1.0, 0.0]],
        "propellant_consumed_kg": null, "isp_s": null, "fill_at_ignition_kg": null, "fill_after_kg": null,
        "tank_psi_at_ignition": null, "chamber": null, "coverage": {"daq_sd": true, "gs_telemetry": true, "daq_rows": 3, "gs_rows": 2},
        "quality_flags": [], "kg1000_impulse_ns": 0.0, "kg50_term_ns": 0.0, "startup_fill_impulse_ns": 0.0, "archive_id": null
    }).to_string()).unwrap();

    let merged = format!("merge-sd.{}", saved.id);
    let ctx = h.context(&merged).await;
    assert_eq!((ctx["source_kind"].as_str(), ctx["recorded_mode"].as_str()), (Some("merge_sd"), Some("test_fire")));
    assert_eq!(ctx["board_sd"][0]["file"], "DAQ_LAUNCH_0001.CSV");
    assert_eq!(ctx["burn_report"]["status"], "final");
    let rows = h.rows(&merged, &["LOADCELL_WEIGHT_KG", "COMBUSTION_CHAMBER_PRESSURE", "DAQ_SD_BOARD_TEMPERATURE"]).await;
    let series = |sender: &str, ty: &str| -> Vec<(i64, f64)> {
        let mut v: Vec<(i64, f64)> = rows.iter().filter(|r| r["sender_id"] == sender && r["data_type"] == ty)
            .map(|r| (r["timestamp_ms"].as_i64().unwrap(), r["values"][0].as_f64().unwrap())).collect();
        v.sort_by_key(|p| p.0);
        v
    };
    assert_eq!(series("DAQ", "LOADCELL_WEIGHT_KG"), vec![(T0 + 1_000, 10.0), (T0 + 2_000, 400.0)], "telemetry kept as recorded");
    assert_eq!(series("DAQ_SD", "LOADCELL_WEIGHT_KG"), vec![(T0 - 2_000, 0.5), (T0 + 1_000, 11.0), (T0 + 1_001, 12.0)], "SD lead-up and launch rows on network time; local-clock rows skipped");
    assert_eq!(series("DAQ_SD", "COMBUSTION_CHAMBER_PRESSURE"), vec![(T0 + 1_500, 350.0)]);
    assert_eq!(series("DAQ_SD", "DAQ_SD_BOARD_TEMPERATURE"), vec![(T0 + 1_600, 31.5)]);

    // The SD files alone, still in the capture's recorded mode.
    let alone = format!("sd-only.{}", saved.id);
    let ctx = h.context(&alone).await;
    assert_eq!(ctx["recorded_mode"], "test_fire");
    let rows = h.rows(&alone, &["LOADCELL_WEIGHT_KG"]).await;
    assert!(!rows.is_empty() && rows.iter().all(|r| r["sender_id"] == "DAQ_SD"), "{rows:?}");

    // The thrust overlay on absolute network time, for the capture and its merges.
    for id in [saved.id.as_str(), merged.as_str(), alone.as_str()] {
        let overlay = h.ok(&format!("/api/recordings/{}/replay/overlays", enc(id))).await;
        assert_eq!(overlay["thrust"]["source"], "burn_report", "{id}");
        assert_eq!(overlay["thrust"]["points"][1], json!([T0 + 1_500, 2000.0]), "{id}");
    }
    // Recorded alerts come back labelled as recorded history.
    for id in [saved.id.as_str(), merged.as_str()] {
        let alerts = h.ok(&format!("/api/recordings/{}/replay/alerts", enc(id))).await;
        assert_eq!(alerts["alerts"], json!([{"timestamp_ms": T0 + 1_200, "severity": "critical", "message": "Chamber pressure high", "recorded": true}]), "{id}");
    }
    // A capture without board SD files has nothing to replay alone.
    let other = materialize(&h.root, &capture(EventKind::Manual, "test_fire", "hardware", T0, T0 + 1_000), T0 + 30_000).await.unwrap();
    assert_eq!(h.get(&format!("/api/recordings/sd-only.{}/replay", enc(&other.id))).await.0, StatusCode::NOT_FOUND);
}

/// A fire archive entry replays its thrust curve (its capture is gone) in the
/// test fire view, with the curve as the overlay.
#[tokio::test]
async fn fire_archive_entry_replays_its_thrust_curve() {
    let _live = thread_mode(Mode::Normal);
    let h = harness().await;
    let dir = h.root.join(crate::fire_archive::DIR).join("fire-2026-11-16");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fire.json"), json!({"id": "fire-2026-11-16", "label": "Static fire", "date": "2026-11-16", "source": "seed",
        "capture_id": "event.launch.gone.db", "peak_thrust_n": 2100.0, "impulse_ns": 4000.0}).to_string()).unwrap();
    std::fs::write(dir.join("thrust.csv"), "t_s,thrust_n\n0.0,0.0\n0.5,2100.0\n1.0,10.0\n").unwrap();
    let ctx = h.context("fire.fire-2026-11-16").await;
    assert_eq!((ctx["source_kind"].as_str(), ctx["default_view"].as_str()), (Some("fire"), Some("test_fire")));
    let rows = h.rows("fire.fire-2026-11-16", &[sources::THRUST_TYPE]).await;
    let thrust: Vec<f64> = rows.iter().map(|r| r["values"][0].as_f64().unwrap()).collect();
    assert_eq!(thrust, vec![0.0, 2100.0, 10.0]);
    let midnight: i64 = 1_794_787_200_000; // 2026-11-16T00:00Z
    assert_eq!(rows[1]["timestamp_ms"], midnight + 500);
    let overlay = h.ok("/api/recordings/fire.fire-2026-11-16/replay/overlays").await;
    assert_eq!(overlay["thrust"]["source"], "fire_archive");
    assert_eq!(overlay["fire_archive"][0]["id"], "fire-2026-11-16");
    assert_eq!(h.get("/api/recordings/fire.nope/replay").await.0, StatusCode::NOT_FOUND);
}

/// Legacy session files from before the GS_STATION_MODE row, the barometer
/// label fix and DAQ_INPUT_HEALTH v2 replay with their mode inferred, raw
/// load-cell rows converted and both health layouts decoded.
#[tokio::test]
async fn legacy_recordings_replay_with_inferred_mode_and_legacy_decoders() {
    let _live = thread_mode(Mode::Normal);
    let h = harness().await;
    let legacy = "groundstation_recording_2026-11-09_06-26-25_821.db";
    let path = h.root.join(legacy);
    // Written without the station schema helper: no loadcell_units mark.
    {
        let db = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display())).await.unwrap();
        sqlx::query("CREATE TABLE telemetry (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp_ms INTEGER NOT NULL, source_timestamp_ms INTEGER, data_type TEXT NOT NULL, sender_id TEXT, values_json TEXT, payload_json TEXT)").execute(&db).await.unwrap();
        let mut v1: Vec<u8> = Vec::new();
        let words_v1: [u32; 10] = [1, 0x0001_0001, 0, 0, 0, 0, 11_000 | (11_100 << 16), 0xFFFF_FFFF, 0xFFFF_FFFF, 8 | (17 << 16)];
        words_v1.iter().for_each(|w| v1.extend(w.to_le_bytes()));
        let mut v2 = v1.clone();
        v2[0] = 2;
        for w in [48u32 | (600 << 16), 0x8000_FFFF, 0x8000_FFFF, 0x8000_FFFF, 0xFFFF_FFFF, 0, 0, 0] {
            v2.extend(w.to_le_bytes());
        }
        for (ts, sender, ty, values, payload) in [
            (T0, "FC", "BAROMETER_DATA", "[-0.64, 98221.9, 24.15]", None),
            (T0 + 10, "DAQ", "DAQ_INPUT_HEALTH", "[1]", Some(serde_json::to_string(&v1).unwrap())),
            (T0 + 20, "DAQ", "DAQ_INPUT_HEALTH", "[2]", Some(serde_json::to_string(&v2).unwrap())),
            (T0 + 30, "FC", "FLIGHT_STATE", "[1]", None),
        ] {
            sqlx::query("INSERT INTO telemetry(timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,?,?,?)")
                .bind(ts).bind(ty).bind(sender).bind(values).bind(payload).execute(&db).await.unwrap();
        }
        db.close().await;
    }
    let ctx = h.context(legacy).await;
    assert_eq!(ctx["source_kind"], "legacy");
    assert_eq!((ctx["recorded_mode"].as_str(), ctx["mode_source"].as_str()), (Some("normal"), Some("inferred")));
    assert_eq!(ctx["provenance"], "unknown");
    assert_eq!(ctx["compatibility"]["daq_input_health_versions"], json!([1, 2]));
    assert_eq!(ctx["compatibility"]["loadcell_units"], "legacy_raw_converted");
    assert_eq!(ctx["compatibility"]["barometer_order"], "alt_prs_tmp");
    let rows = h.rows(legacy, &["DAQ_INPUT_HEALTH", "BAROMETER_DATA"]).await;
    let health: Vec<&Value> = rows.iter().filter(|r| r["data_type"] == "DAQ_INPUT_HEALTH").collect();
    assert_eq!(health[0]["decoded"]["version"], 1);
    assert!(health[0]["decoded"].get("noise").is_none());
    assert_eq!(health[1]["decoded"]["version"], 2);
    assert_eq!(health[1]["decoded"]["noise"][0]["noise_uv"], 48);
    let baro = rows.iter().find(|r| r["data_type"] == "BAROMETER_DATA").unwrap();
    let values: Vec<f64> = baro["values"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    for (got, want) in values.iter().zip([-0.64, 98221.9, 24.15]) {
        assert!((got - want).abs() < 0.01, "raw values in FC order: {values:?}");
    }
    // The shipped layout labels them in that order.
    let layout = h.layout(legacy, None).await;
    let tab = layout["data_tab"]["tabs"].as_array().unwrap().iter().find(|t| t["id"] == "BAROMETER_DATA").unwrap();
    assert_eq!(tab["channels"], json!(["Altitude", "Pressure", "Temp"]));
}

/// Bytes 14 and 15 were EvaluationRelax and EvaluationFocus, retired with
/// the FC's estimator guard. A HITL recording that carries them still replays
/// them as ordinary FLIGHT_COMMAND rows; nothing parses them as a command.
#[tokio::test]
async fn hitl_recordings_with_the_retired_evaluation_commands_still_replay() {
    let _live = thread_mode(Mode::Normal);
    let h = harness().await;
    let payload = mode_row("hitl", "hardware", false, false);
    let mut rows = mode_rows(&payload, "hitl");
    rows.extend([
        (T0 + 3_500, "GS", "FLIGHT_COMMAND", Some("[14]"), None),
        (T0 + 3_600, "GS", "FLIGHT_COMMAND", Some("[15]"), None),
        (T0 + 3_700, "GS", "FLIGHT_COMMAND", Some("[16]"), None),
    ]);
    let segment = write_segment(&h.root, T0 - 1_000, &rows).await;
    let ctx = h.context(&segment).await;
    assert_eq!(ctx["recorded_mode"], "hitl");
    let commands: Vec<Value> = h
        .rows(&segment, &["FLIGHT_COMMAND"])
        .await
        .into_iter()
        .map(|r| r["values"].clone())
        .collect();
    assert_eq!(commands, vec![json!([14.0]), json!([15.0]), json!([16.0])]);
    // The live command set no longer has them.
    for retired in ["EvaluationRelax", "EvaluationFocus"] {
        assert!(serde_json::from_value::<crate::types::TelemetryCommand>(json!(retired)).is_err());
    }
}

/// A layout saved before the label fix (in an SD import manifest or portable
/// replay) is relabelled; the shipped layouts are already right.
#[test]
fn layouts_saved_before_the_barometer_fix_are_relabelled() {
    let mut old = json!({
        "data_tab": {"tabs": [{"id": "BAROMETER_DATA", "label": "Barometer Data", "channels": ["Pressure", "Temp", "Altitude"]}]},
        "state_tab": {"states": [{"sections": [{"widgets": [
            {"data_type": "BAROMETER_DATA", "items": [{"label": "Pressure", "index": 0}, {"label": "Temp", "index": 1}, {"label": "Baro Alt", "index": 2}]},
            {"chart_series": [{"data_type": "BAROMETER_DATA", "index": 2, "label": "Barometer"}]}
        ]}]}]}
    });
    assert!(crate::layout::relabel_legacy_barometer(&mut old));
    assert_eq!(old["data_tab"]["tabs"][0]["channels"], json!(["Altitude", "Pressure", "Temp"]));
    let items = &old["state_tab"]["states"][0]["sections"][0]["widgets"][0]["items"];
    assert_eq!(items, &json!([{"label": "Pressure", "index": 1}, {"label": "Temp", "index": 2}, {"label": "Baro Alt", "index": 0}]));
    assert_eq!(old["state_tab"]["states"][0]["sections"][0]["widgets"][1]["chart_series"][0]["index"], 0);
    for file in ["layout.json", "layout_hitl.json", "layout_test_fire.json"] {
        let mut shipped: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("layout").join(file)).unwrap()).unwrap();
        assert!(!crate::layout::relabel_legacy_barometer(&mut shipped), "{file} is already in FC order");
    }
}

/// Replay of every source kind sends no command, writes nothing to the
/// recording queue, broadcasts no live telemetry, starts no capture, raises
/// no notification, leaves the saved files unchanged and the live mode as is.
#[tokio::test]
async fn replay_has_no_command_capture_or_live_side_effects() {
    let _live = thread_mode(Mode::Normal);
    let root = TestDirectory::new();
    sources::clear_cache().await;
    let mut state = export_state(&root.0, true).await;
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
    let (db_tx, mut db_rx) = tokio::sync::mpsc::channel(16);
    {
        let inner = Arc::get_mut(&mut state).unwrap();
        inner.cmd_tx = cmd_tx;
        inner.db_queue_tx = db_tx;
    }
    let mut ws_rx = state.ws_tx.subscribe();
    let mut flight_rx = state.state_tx.subscribe();
    let mut notification_rx = state.notifications_tx.subscribe();
    let notifications_before = state.notifications_snapshot().len();
    let app = crate::recording_export::routes().with_state(state.clone());
    let h = Harness { root: root.0.clone(), _root: root, app };

    // An abort-and-launch flavoured IGT recording: flight states, alerts, SIM rows.
    let igt = mode_row("integrated_ground_test", "hardware", true, false);
    let mut rows = mode_rows(&igt, "integrated_ground_test");
    rows.push((T0 + 3_500, "FC", "FLIGHT_STATE", Some("[16]"), None));
    rows.push((T0 + 3_600, "GS", "ABORT", Some("[1]"), None));
    let segment = write_segment(&h.root, T0, &rows).await;
    let saved = materialize(&h.root, &capture(EventKind::Launch, "integrated_ground_test", "hardware", T0 + 3_000, T0 + 3_600), T0 + 20_000).await.unwrap();
    attach_board_file(&saved.folder, "DAQ", "DAQ_LAUNCH_0002.CSV", &[format!("{},1,kg1000_network,1.0,,,,network,,,1,1000000,0", T0 + 3_100)], None);
    let fingerprint = |dir: &Path| -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let m = e.metadata().unwrap();
                if m.is_dir() { stack.push(e.path()) } else { out.push((e.path(), m.len(), m.modified().unwrap())) }
            }
        }
        out.sort();
        out
    };
    let recordings = h.root.join("recordings");
    let before = fingerprint(&recordings);

    let ids = [segment.clone(), saved.id.clone(), format!("merge-sd.{}", saved.id), format!("sd-only.{}", saved.id), format!("range.{}.{}", T0, T0 + 10_000)];
    for id in &ids {
        for suffix in ["replay", "replay/context", "replay/layout", "replay/layout?view=test_fire", "replay/overlays", "replay/alerts", "replay/flight", "replay/export"] {
            let (status, value) = h.get(&format!("/api/recordings/{}/{suffix}", enc(id))).await;
            assert_eq!(status, StatusCode::OK, "{id} {suffix}: {value}");
        }
        let _ = h.rows(id, &["FLIGHT_STATE", "ABORT", "SIM_SAMPLE", "LOADCELL_WEIGHT_KG"]).await;
    }
    assert!(cmd_rx.try_recv().is_err(), "replay sent a command");
    assert!(db_rx.try_recv().is_err(), "replay wrote to the live recording");
    assert!(ws_rx.try_recv().is_err(), "replay broadcast live telemetry");
    assert!(flight_rx.try_recv().is_err(), "replay changed the live flight state");
    assert!(notification_rx.try_recv().is_err(), "replay raised a notification");
    assert_eq!(state.notifications_snapshot().len(), notifications_before);
    assert!(state.recording_store.captures().is_empty(), "replay started a capture");
    assert_eq!(fingerprint(&recordings), before, "replay modified saved files");
    assert!(!saved.folder.join(crate::burn_report::REPORT_FILE).exists(), "replay produced a burn report");
    assert!(!h.root.join(crate::fire_archive::DIR).exists(), "replay produced a fire archive record");
    assert_eq!(crate::runtime_mode::current(), Mode::Normal);
    assert!(std::fs::metadata(saved.folder.join(CAPTURE_FILE)).is_ok());
}

#[test]
fn replay_ids_name_every_source_kind_and_reject_paths() {
    assert_eq!(Source::parse("range.10.20"), Some(Source::Range { start_ms: 10, end_ms: 20 }));
    assert_eq!(Source::parse("range.20.10"), None);
    assert_eq!(Source::parse("range.1.2.3"), None);
    assert_eq!(Source::parse("merge-sd.event.launch.2026_fire.db"), Some(Source::MergeSd("event.launch.2026_fire.db".into())));
    assert_eq!(Source::parse("merge-sd.event.launch.2026_fire.DAQ.db"), None, "captures only, not attachments");
    assert_eq!(Source::parse("sd-only.groundstation_recording_1.db"), None);
    assert_eq!(Source::parse("fire.fire-1"), Some(Source::Fire("fire-1".into())));
    assert_eq!(Source::parse("fire.../x"), None);
    assert_eq!(Source::parse("rolling.segment_2026-11-09_06-26-25_821.db").map(|s| s.kind()), Some("rolling"));
    assert_eq!(Source::parse("../etc/passwd"), None);
    assert_eq!(sources::sd_row("FC", "1800000000000,5,accelerometer_sample,\"1.5 2 3\",,,,network,,,5,1000,0"),
        Some(Ok(sources::SdRow { timestamp_ms: 1_800_000_000_000, data_type: "FC_SD_ACCELEROMETER_SAMPLE".into(), sender_id: "FC_SD".into(), values: vec![1.5, 2.0, 3.0] })));
    assert_eq!(sources::sd_row("FC", "5,5,accelerometer_sample,1,,,,local,,,5,1000,0"), Some(Err(())));
    assert_eq!(sources::sd_row("DAQ", "# calibration,ch0,1,0"), None);
}
