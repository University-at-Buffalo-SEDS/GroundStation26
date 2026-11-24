use super::merge::{Channel, Sample, Source};
use super::thrust::{self, Config, G};
use super::*;

const WINDOW: &str = include_str!("fixtures/fire_20261116_window.csv");

fn window_channel(name: &str) -> Vec<(f64, f64)> {
    WINDOW
        .lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("t_s"))
        .filter_map(|l| {
            let mut f = l.split(',');
            let t = f.next()?.parse::<f64>().ok()?;
            (f.next()? == name).then_some(())?;
            Some((t, f.next()?.parse::<f64>().ok()?))
        })
        .collect()
}

#[test]
fn real_fire_matches_the_python_extraction_with_the_kg50_unloading() {
    let k1k = window_channel("kg1000_kg");
    let k50 = window_channel("kg50_kg");
    let t = thrust::extract(&k1k, &k50, &Config::default()).unwrap();
    // tools/flight_profile/extract_thrust.py: 9192.0 N s nominal.
    assert!(
        (t.total_impulse_ns - 9192.0).abs() / 9192.0 < 0.02,
        "{}",
        t.total_impulse_ns
    );
    assert!(
        (t.kg50_pre_kg - 5.65).abs() < 0.05 && (t.kg50_plateau_kg + 38.2).abs() < 0.5,
        "{t:?}"
    );
    assert!((t.kg50_post_kg + 6.56).abs() < 0.1);
    assert!(t.lifted);
    assert!(
        t.kg50_term_ns > 1800.0 && t.kg50_term_ns < 2400.0,
        "KG50 term {}",
        t.kg50_term_ns
    );
    assert!((t.burn_time_s - 5.65).abs() < 0.3, "{}", t.burn_time_s);
    let codes: Vec<&str> = t.flags.iter().map(|f| f.code.as_str()).collect();
    for code in [
        "kg1000_clamp",
        "kg1000_startup_gap",
        "kg50_supply_dip_spikes",
        "drop_back",
    ] {
        assert!(codes.contains(&code), "{code} missing from {codes:?}");
    }
    assert!(
        t.drop_back_s.is_some_and(|d| (5.2..6.0).contains(&d)),
        "{:?}",
        t.drop_back_s
    );
    assert!(
        t.impulse_sigma_ns > 500.0 && t.impulse_sigma_ns < 1500.0,
        "{}",
        t.impulse_sigma_ns
    );
    // The KG1000 alone (the earlier method) undercounts by about 2 kN s.
    let alone = thrust::extract(
        &k1k,
        &k50,
        &Config {
            kg50_scale: 0.0,
            ..Config::default()
        },
    )
    .unwrap();
    assert!(
        (alone.total_impulse_ns - 7106.0).abs() / 7106.0 < 0.03,
        "{}",
        alone.total_impulse_ns
    );
}

/// A stand burn sampled at `hz`: the rocket of `hanging_kg` (with
/// `prop_kg` of it propellant) hangs from KG50, whose tare reads `tare` at
/// zero load; once thrust exceeds the weight it lifts, crosses a `gap_s`
/// gap with neither cell loaded, then presses KG1000 (with a contact spike).
struct Stand {
    hanging_kg: f64,
    prop_kg: f64,
    tare: f64,
    gap_s: f64,
    hz: f64,
}

fn burn(stand: &Stand, thrust_n: impl Fn(f64) -> f64) -> (Vec<(f64, f64)>, Vec<(f64, f64)>, f64) {
    let dt_fine = 0.0005;
    let mut total = 0.0;
    let mut cum = vec![];
    let mut t = 0.0;
    while t < 20.0 {
        total += thrust_n(t) * dt_fine;
        cum.push(total);
        t += dt_fine;
    }
    let burned = |t: f64| {
        let i = ((t / dt_fine) as usize).min(cum.len() - 1);
        if t <= 0.0 {
            0.0
        } else {
            stand.prop_kg * cum[i] / total
        }
    };
    let (mut k50, mut k1k) = (vec![], vec![]);
    let mut lifted_at: Option<f64> = None;
    let mut t = -5.0;
    while t <= 20.0 {
        let m = stand.hanging_kg - burned(t);
        let th = if t > 0.0 { thrust_n(t) } else { 0.0 };
        let net = th / G - m;
        if net > 0.0 && lifted_at.is_none() {
            lifted_at = Some(t);
        }
        if net <= 0.0 {
            lifted_at = None;
        }
        let in_gap = lifted_at.is_some_and(|l| t - l < stand.gap_s);
        let r = if net > 0.0 {
            stand.tare
        } else {
            stand.tare + m - th / G
        };
        let mut f = if net > 0.0 && !in_gap { net } else { 0.0 };
        if lifted_at.is_some_and(|l| (t - l - stand.gap_s).abs() < 0.5 / stand.hz) {
            f += 40.0; // contact spike as the gap closes
        }
        k50.push((t, r));
        k1k.push((t, f));
        t += 1.0 / stand.hz;
    }
    (k1k, k50, total)
}

fn config_for_synthetic() -> Config {
    // A healthy KG1000 reads its baseline from the start: no startup bridge.
    Config {
        post_window: (12.0, 20.0),
        ..Config::default()
    }
}

#[test]
fn synthetic_lifted_burn_recovers_the_known_impulse() {
    let stand = Stand {
        hanging_kg: 40.0,
        prop_kg: 6.0,
        tare: -32.0,
        gap_s: 0.04,
        hz: 200.0,
    };
    let profile = |t: f64| {
        if t < 0.1 {
            0.0
        } else if t < 0.3 {
            2500.0 * (t - 0.1) / 0.2
        } else if t < 3.0 {
            2500.0 - 200.0 * (t - 0.3)
        } else if t < 6.0 {
            1960.0 * (1.0 - (t - 3.0) / 3.0)
        } else {
            0.0
        }
    };
    let (k1k, k50, truth) = burn(&stand, profile);
    let t = thrust::extract(&k1k, &k50, &config_for_synthetic()).unwrap();
    assert!(
        (t.total_impulse_ns - truth).abs() / truth < 0.015,
        "{} vs {truth}",
        t.total_impulse_ns
    );
    assert!(t.lifted && t.startup_fill_impulse_ns == 0.0, "{t:?}");
    assert!(
        t.flags.iter().any(|f| f.code == "gap_closing_transient"),
        "{:?}",
        t.flags
    );
    assert!(t.flags.iter().any(|f| f.code == "drop_back"));
    // The KG1000 alone misses the hanging weight and the tail.
    let alone = thrust::extract(
        &k1k,
        &k50,
        &Config {
            kg50_scale: 0.0,
            ..config_for_synthetic()
        },
    )
    .unwrap();
    assert!(alone.total_impulse_ns < 0.8 * truth);
}

#[test]
fn synthetic_burn_below_the_hanging_weight_is_kg50_only() {
    let stand = Stand {
        hanging_kg: 40.0,
        prop_kg: 1.0,
        tare: 0.0,
        gap_s: 0.04,
        hz: 40.0,
    };
    let profile = |t: f64| {
        if (0.2..4.2).contains(&t) {
            350.0 - 50.0 * (t - 0.2)
        } else {
            0.0
        }
    };
    let (k1k, k50, truth) = burn(&stand, profile);
    let t = thrust::extract(&k1k, &k50, &config_for_synthetic()).unwrap();
    assert!(!t.lifted);
    assert!(
        (t.total_impulse_ns - truth).abs() / truth < 0.03,
        "{} vs {truth}",
        t.total_impulse_ns
    );
    assert!(t.flags.iter().any(|f| f.code == "not_lifted"));
}

fn s(t_ms: i64, value: f64, source: Source) -> Sample {
    Sample {
        t_ms,
        value,
        source,
    }
}

#[test]
fn merge_prefers_daq_and_fills_its_holes_from_telemetry() {
    // DAQ at 400 Hz with a hole 1000-1600 ms; GS at 40 Hz, 7 ms offset.
    let daq: Vec<Sample> = (0..800)
        .map(|i| i * 25 / 10)
        .filter(|t| !(1000..1600).contains(t))
        .map(|t| s(t, 1.0, Source::DaqSd))
        .collect();
    let gs: Vec<Sample> = (0..80)
        .map(|i| s(i * 25 + 7, 2.0, Source::GsTelemetry))
        .collect();
    let merged = merge::merge(&daq, &gs);
    assert!(merged.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
    let gs_used: Vec<i64> = merged
        .iter()
        .filter(|x| x.source == Source::GsTelemetry)
        .map(|x| x.t_ms)
        .collect();
    assert!(!gs_used.is_empty());
    assert!(
        gs_used.iter().all(|t| (1000..1600).contains(t)),
        "{gs_used:?}"
    );
    assert_eq!(
        merged.iter().filter(|x| x.source == Source::DaqSd).count(),
        daq.len()
    );
    // GS samples beyond the end of the DAQ file are kept too.
    let short: Vec<Sample> = daq.iter().copied().filter(|x| x.t_ms < 500).collect();
    let merged = merge::merge(&short, &gs);
    assert!(
        merged
            .iter()
            .any(|x| x.source == Source::GsTelemetry && x.t_ms > 1900)
    );
}

#[test]
fn daq_rows_need_network_time_and_a_known_sensor() {
    let row =
        merge::parse_daq_line("1794877195300,1234,kg50_network,\"-38.2\",,,,network,,,1234,1000,9")
            .unwrap();
    assert_eq!(row.0, Channel::Kg50);
    assert_eq!(
        (row.1.t_ms, row.1.value, row.1.source),
        (1794877195300, -38.2, Source::DaqSd)
    );
    let calibrated =
        merge::parse_daq_line("1794877195300,1234,kg1000_network,,1,2,250.5,network,,,1234,1000,9")
            .unwrap();
    assert_eq!(calibrated.1.value, 250.5);
    assert!(merge::parse_daq_line("1234,1234,kg50_network,1.0,,,,local,,,1234,1000,9").is_none());
    assert!(
        merge::parse_daq_line("1794877195300,1234,vmon_v,1.0,,,,network,,,1234,1000,9").is_none()
    );
}

// ---------------------------------------------------------------- folders

pub(crate) const T0: i64 = 1_800_000_000_000;

pub(crate) fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "seds-burn-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn manifest(folder: &str, mode: &str, details: serde_json::Value) -> Manifest {
    Manifest {
        format: "seds-gs-event-capture".into(),
        version: 1,
        id: crate::recording_store::RecordingId::event_id(EventKind::Launch, folder),
        event_type: EventKind::Launch,
        title: "Launch".into(),
        label: "Launch".into(),
        event_start_ms: T0 - 2_000,
        event_end_ms: T0 + 60_000,
        window_start_ms: T0 - 60_000,
        window_end_ms: T0 + 90_000,
        pre_roll_s: 58,
        post_roll_s: 30,
        station_mode: mode.into(),
        versions: serde_json::json!({}),
        operator: None,
        result: Some("Landed".into()),
        rows: Default::default(),
        data_from_ms: None,
        data_to_ms: None,
        pre_roll_truncated: false,
        source_segments: vec![],
        saved_at_ms: T0 + 90_000,
        details,
        board_sd: vec![],
        provenance: Some("hardware".into()),
        simulated: false,
    }
}

/// A capture folder holding a synthetic stand burn in its telemetry (40 Hz)
/// and, optionally, a DAQ launch file (200 Hz).
pub(crate) async fn capture_folder(root: &Path, name: &str, mode: &str, with_daq: bool) -> PathBuf {
    let folder = crate::recording_store::events_dir(root)
        .join("launch")
        .join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let stand = Stand {
        hanging_kg: 40.0,
        prop_kg: 6.0,
        tare: -34.0,
        gap_s: 0.03,
        hz: 40.0,
    };
    let profile = |t: f64| if (0.1..5.0).contains(&t) { 2200.0 } else { 0.0 };
    let (k1k, k50, _) = burn(&stand, profile);
    let (db, _) = crate::telemetry_db::open_telemetry_db(&folder.join(CAPTURE_FILE))
        .await
        .unwrap();
    let insert = |t: f64, kind: &str, sender: &str, value: String| {
        let ms = T0 + (t * 1000.0).round() as i64;
        (ms, kind.to_owned(), sender.to_owned(), value)
    };
    let mut rows = vec![insert(
        0.0,
        "VALVE_STATE",
        "VB",
        "[1.0,0.0,0.0,1.0,0.0,0.0,0.0,null]".into(),
    )];
    // Telemetry drops 2-3 s of the KG1000 (a radio gap the DAQ file fills).
    rows.extend(
        k1k.iter()
            .filter(|(t, _)| !(2.0..3.0).contains(t))
            .map(|(t, v)| insert(*t, "LOADCELL_WEIGHT_KG", "DAQ", format!("[{v}]"))),
    );
    rows.extend(
        k50.iter()
            .map(|(t, v)| insert(*t, "LOADCELL_50_WEIGHT_KG", "DAQ", format!("[{v}]"))),
    );
    rows.extend((-200..800).map(|i| {
        insert(
            i as f64 * 0.025,
            "PRESSURE_TRANSDUCER_CALIBRATED",
            "VB",
            "[400.0]".into(),
        )
    }));
    for (ms, kind, sender, value) in rows {
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,?,?,?,'[]')")
            .bind(ms + 40)
            .bind(ms)
            .bind(kind)
            .bind(sender)
            .bind(value)
            .execute(&db)
            .await
            .unwrap();
    }
    db.close().await;
    let mut m = manifest(
        name,
        mode,
        serde_json::json!({"trigger_flight_state": 7, "apogee_plan": {"plan": {"weather": {"surface_temp_c": 18.5}}}}),
    );
    if with_daq {
        add_daq_file(&folder, &mut m, &stand, profile);
    }
    std::fs::write(
        folder.join(MANIFEST_FILE),
        serde_json::to_vec_pretty(&m).unwrap(),
    )
    .unwrap();
    folder
}

fn add_daq_file(folder: &Path, m: &mut Manifest, stand: &Stand, profile: impl Fn(f64) -> f64) {
    let fast = Stand {
        hz: 200.0,
        ..*stand
    };
    let (k1k, k50, _) = burn(&fast, profile);
    let dir = folder
        .join(crate::recording_store::board_sd::BOARD_SD_DIR)
        .join("DAQ");
    std::fs::create_dir_all(&dir).unwrap();
    let mut csv = String::from(
        "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence\n",
    );
    for (i, ((t, f), (_, r))) in k1k
        .iter()
        .zip(&k50)
        .enumerate()
        .filter(|(_, ((t, _), _))| *t <= 10.0)
    {
        let ms = T0 + (t * 1000.0).round() as i64;
        csv.push_str(&format!(
            "{ms},{i},kg1000_network,\"{f}\",,,,network,,,{i},1000,{}\n",
            2 * i
        ));
        csv.push_str(&format!(
            "{ms},{i},kg50_network,\"{r}\",,,,network,,,{i},1000,{}\n",
            2 * i + 1
        ));
    }
    std::fs::write(dir.join("DAQ_LAUNCH_0001.CSV"), &csv).unwrap();
    m.board_sd
        .push(crate::recording_store::board_sd::BoardSdEntry {
            board: "DAQ".into(),
            file: "DAQ_LAUNCH_0001.CSV".into(),
            bytes: csv.len() as u64,
            sha256: "x".into(),
            data_from_ms: T0 - 5_000,
            data_to_ms: T0 + 10_000,
            rows: 0,
            matched_by: "utc_window".into(),
            source: "network_download".into(),
            lead_up_file: None,
            lead_up_from_ms: None,
            lead_up_rows: 0,
            link: None,
            sd_format: None,
            relay: None,
            igt_dataset: None,
        });
}

#[tokio::test]
async fn provisional_report_becomes_final_when_the_daq_file_arrives() {
    let root = temp_root("flow");
    let folder = capture_folder(&root, "2027-01-15_08-00-00_launch", "test_fire", false).await;
    let provisional = recompute_folder_in(&root, &folder, Some(700.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(provisional.status, Status::Provisional);
    assert!(
        !provisional.simulated && provisional.coverage.gs_telemetry && !provisional.coverage.daq_sd
    );
    assert!(
        provisional
            .quality_flags
            .iter()
            .any(|f| f.code == "telemetry_only")
    );
    assert_eq!(provisional.ignition_source, "main_valve_open");
    let truth = 2200.0 * 4.9;
    assert!(
        (provisional.total_impulse_ns - truth).abs() / truth < 0.05,
        "{}",
        provisional.total_impulse_ns
    );
    assert!(folder.join(REPORT_FILE).exists() && folder.join(CURVE_FILE).exists());
    let id = provisional.archive_id.clone().expect("archived");
    let fire = crate::fire_archive::get(&root, &id).unwrap();
    assert_eq!(fire.review.status, "pending");
    assert_eq!(
        (fire.ambient_c, fire.supercharge_psi),
        (Some(18.5), Some(700.0))
    );
    assert!(!fire.in_fit, "pending fires stay out of the fit");
    // Same data again: no new notification, nothing changes.
    assert!(
        recompute_folder_in(&root, &folder, Some(700.0))
            .await
            .unwrap()
            .is_none()
    );

    // The DAQ file is attached: final, from the merged series.
    let mut m = crate::recording_store::read_manifest(&folder).unwrap();
    let stand = Stand {
        hanging_kg: 40.0,
        prop_kg: 6.0,
        tare: -34.0,
        gap_s: 0.03,
        hz: 40.0,
    };
    add_daq_file(&folder, &mut m, &stand, |t: f64| {
        if (0.1..5.0).contains(&t) { 2200.0 } else { 0.0 }
    });
    std::fs::write(
        folder.join(MANIFEST_FILE),
        serde_json::to_vec_pretty(&m).unwrap(),
    )
    .unwrap();
    let final_report = recompute_folder_in(&root, &folder, Some(700.0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_report.status, Status::Final);
    assert!(final_report.coverage.daq_sd && final_report.coverage.daq_rows > 1000);
    assert!(
        !final_report
            .quality_flags
            .iter()
            .any(|f| f.code == "telemetry_only")
    );
    assert!(
        (final_report.total_impulse_ns - truth).abs() / truth < 0.03,
        "{}",
        final_report.total_impulse_ns
    );
    assert_eq!(
        final_report.archive_id.as_deref(),
        Some(id.as_str()),
        "the same fire record is updated"
    );
    assert_eq!(read_report(&folder).unwrap().status, Status::Final);
    // Listing and lookup by id.
    let listed = list(&root);
    assert!(
        listed
            .iter()
            .any(|r| r.id == final_report.id && r.status == Status::Final)
    );
    assert_eq!(
        find(&root, &final_report.id).unwrap().total_impulse_ns,
        final_report.total_impulse_ns
    );
    assert!(find(&root, "launch.../etc").is_none() && find(&root, "abort.x").is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hitl_reports_are_simulated_and_kept_out_of_the_fit() {
    let root = temp_root("modes");
    for (i, (mode, simulated)) in [
        ("hitl", true),
        ("test_fire", false),
        ("integrated_ground_test", false),
        ("normal", false),
    ]
    .into_iter()
    .enumerate()
    {
        let folder =
            capture_folder(&root, &format!("2027-01-1{i}_08-00-00_launch"), mode, true).await;
        let report = recompute_folder_in(&root, &folder, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            report.simulated,
            simulated || crate::flight_sim_runtime::enabled(),
            "{mode}"
        );
        assert_eq!(report.mode, mode);
        let fire = crate::fire_archive::get(&root, report.archive_id.as_ref().unwrap()).unwrap();
        assert_eq!(fire.simulated, report.simulated);
        assert_eq!(fire.review.exclude_from_fit, report.simulated, "{mode}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn saved_flight_captures_queue_a_report_in_every_mode() {
    use crate::station_mode::Mode;
    for mode in [
        Mode::TestFire,
        Mode::IntegratedGroundTest,
        Mode::Hitl,
        Mode::Normal,
    ] {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let folder = PathBuf::from(format!("/tmp/burn-queue-test-{mode:?}"));
        let capture = crate::recording_store::Capture {
            serial: 999_000 + mode as u64,
            kind: if mode == Mode::Hitl {
                EventKind::Hitl
            } else {
                EventKind::Launch
            },
            key: "flight".into(),
            label: "Launch".into(),
            event_start_ms: 0,
            window_start_ms: 0,
            event_end_ms: Some(1),
            post_roll_ms: 0,
            max_end_ms: 1,
            result: None,
            operator: None,
            station_mode: crate::station_mode_switch::mode_id(mode).into(),
            details: serde_json::Value::Null,
            materializing: false,
            provenance: Some("hardware".into()),
        };
        capture_saved(&capture, &folder);
        assert!(RUNTIME.lock().unwrap().queue.contains(&folder), "{mode:?}");
        RUNTIME.lock().unwrap().queue.retain(|f| f != &folder);
    }
    // Other capture kinds never get a burn report.
    let mut other = crate::recording_store::Capture {
        serial: 1,
        kind: EventKind::Fill,
        key: "x".into(),
        label: "Fill".into(),
        event_start_ms: 0,
        window_start_ms: 0,
        event_end_ms: None,
        post_roll_ms: 0,
        max_end_ms: 0,
        result: None,
        operator: None,
        station_mode: "normal".into(),
        details: serde_json::Value::Null,
        materializing: false,
        provenance: None,
    };
    let folder = PathBuf::from("/tmp/burn-queue-test-fill");
    capture_saved(&other, &folder);
    assert!(!RUNTIME.lock().unwrap().queue.contains(&folder));
    other.kind = EventKind::Abort;
    capture_saved(&other, &folder);
    assert!(!RUNTIME.lock().unwrap().queue.contains(&folder));
}

/// The 2026-11-16 fire as a report computed from the committed window (valve open at t = 0).
pub(crate) fn real_fire_report() -> BurnReport {
    let t0 = 1_800_000_000_000i64;
    let mut inputs = Inputs { valve_open_ms: vec![t0], ..Default::default() };
    for (name, ty, sender) in [("kg1000_kg", "LOADCELL_WEIGHT_KG", "DAQ"), ("kg50_kg", "LOADCELL_50_WEIGHT_KG", "DAQ"), ("tank_psi", "PRESSURE_TRANSDUCER_CALIBRATED", "VB")] {
        for (t, v) in window_channel(name) {
            inputs.push_gs(t0 + (t * 1000.0).round() as i64, ty, sender, &[Some(v)]);
        }
    }
    let meta = Meta { id: "fire".into(), capture_id: None, capture_label: "fire".into(), mode: "test_fire".into(), simulated: false, created_ms: 0 };
    compute(&inputs, meta).unwrap()
}

#[test]
fn reports_keep_the_tank_trace_and_the_hybrid_model_prediction() {
    let r = real_fire_report();
    assert!(r.tank_curve.len() > 60, "0.1 s tank medians: {}", r.tank_curve.len());
    assert!((r.tank_curve[0][1] - 404.0).abs() < 8.0, "{:?}", r.tank_curve[0]);
    let p = r.predicted.as_ref().expect("the model's prediction");
    assert!((p.fill_kg - 5.65).abs() < 0.05 && (p.tank_c + 3.1).abs() < 0.5, "{p:?}");
    assert!((p.impulse_ns / 9192.0 - 1.0).abs() < 0.10, "{}", p.impulse_ns);
    assert_eq!(p.tank_curve.len(), p.curve.len());
    assert!(r.chamber_curve.is_empty(), "no chamber PT in this window");
}

#[test]
fn no_burn_means_no_report() {
    let mut inputs = Inputs::default();
    for i in 0..400 {
        inputs.push_gs(T0 + i * 25, "LOADCELL_50_WEIGHT_KG", "DAQ", &[Some(5.0)]);
        inputs.push_gs(T0 + i * 25, "LOADCELL_WEIGHT_KG", "DAQ", &[Some(0.0)]);
    }
    let meta = Meta {
        id: "x".into(),
        capture_id: None,
        capture_label: "x".into(),
        mode: "normal".into(),
        simulated: false,
        created_ms: 0,
    };
    assert!(compute(&inputs, meta).is_err());
}

#[test]
fn kg50_floor_ignores_the_burnt_out_hanging_level() {
    let stand = Stand {
        hanging_kg: 40.0,
        prop_kg: 6.0,
        tare: -32.0,
        gap_s: 0.04,
        hz: 200.0,
    };
    let profile = |t: f64| if (0.1..5.0).contains(&t) { 2000.0 } else { 0.0 };
    let (_, k50, _) = burn(&stand, profile);
    let (floor, flat_s) = thrust::kg50_floor_for_test(&k50, 8.0, 2.0);
    assert_eq!(floor, -32.0);
    assert!(flat_s > 4.0, "{flat_s}");
}

#[test]
fn firmware_sim_captures_are_simulated_in_any_mode() {
    let mut m = manifest("2027-01-01_08-00-00_launch", "test_fire", serde_json::Value::Null);
    assert!(!meta_for(&m, "f", 0).simulated || crate::flight_sim_runtime::enabled());
    m.provenance = Some("firmware_sim".into());
    assert!(meta_for(&m, "f", 0).simulated);
    m.provenance = Some("hardware".into());
    m.simulated = true;
    assert!(meta_for(&m, "f", 0).simulated);
}
