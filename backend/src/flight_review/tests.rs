use super::data::{Data, Row};
use super::flight::{Chute, compute, distance_bearing, parse_deployment, speed_of_sound};
use super::*;
use crate::burn_report::tests::{T0, capture_folder, manifest, temp_root};

fn row(t_s: f64, data_type: &str, sender: &str, values: &[f64]) -> Row {
    Row {
        t_ms: T0 + (t_s * 1000.0).round() as i64,
        data_type: data_type.into(),
        sender: sender.into(),
        values: values.iter().map(|v| Some(*v)).collect(),
        payload: None,
    }
}

fn ms(t_s: f64) -> i64 {
    T0 + (t_s * 1000.0).round() as i64
}

/// A synthetic flight on T-0 = main valve open: liftoff at 0.3 s, a 4 s
/// boost at 50 m/s² net, coast to apogee (~2442 m at ~24.7 s), drogue at
/// 25.7 s (25 m/s), main at 300 m (6 m/s), touchdown.
struct Flight {
    apogee_t: f64,
    apogee_h: f64,
    drogue_t: f64,
    main_t: f64,
    land_t: f64,
}

fn profile() -> Flight {
    let (boost_end, v_bo, h_bo) = (4.3, 200.0, 400.0);
    let tau = v_bo / 9.8;
    let apogee_t = boost_end + tau;
    let apogee_h = h_bo + v_bo * tau - 4.9 * tau * tau;
    let drogue_t = apogee_t + 1.0;
    let h_drogue = apogee_h - 4.9;
    let main_t = drogue_t + (h_drogue - 300.0) / 25.0;
    let land_t = main_t + 300.0 / 6.0;
    Flight {
        apogee_t,
        apogee_h,
        drogue_t,
        main_t,
        land_t,
    }
}

fn height(f: &Flight, t: f64) -> (f64, f64) {
    if t < 0.3 {
        (0.0, 0.0)
    } else if t < 4.3 {
        let u = t - 0.3;
        (25.0 * u * u, 50.0 * u)
    } else if t < f.drogue_t {
        let u = t - 4.3;
        (400.0 + 200.0 * u - 4.9 * u * u, 200.0 - 9.8 * u)
    } else if t < f.main_t {
        (f.apogee_h - 4.9 - 25.0 * (t - f.drogue_t), -25.0)
    } else if t < f.land_t {
        (300.0 - 6.0 * (t - f.main_t), -6.0)
    } else {
        (0.0, 0.0)
    }
}

fn flight_data(f: &Flight) -> Data {
    let mut d = Data::default();
    d.station_states.push((ms(-3.0), 7));
    d.rows.push(row(-3.0, "LAUNCH_SITE", "GS", &[43.0, -78.8, 200.0]));
    d.rows.push(row(0.0, "VALVE_STATE", "VB", &[1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]));
    let mut t = -10.0;
    while t < f.land_t + 20.0 {
        let (h, v) = height(f, t);
        if t >= 0.0 {
            if t < f.apogee_t {
                d.rows.push(row(t, "ASCENT_STATE", "FC", &[1.0, 0.0, 0.0, 0.0, h, v]));
            } else {
                d.rows.push(row(t, "DESCENT_STATE", "FC", &[43.0, -78.8, h, v]));
            }
        }
        d.rows.push(row(t, "BAROMETER_DATA", "FC", &[h, 101_325.0, 20.0]));
        let a = if (0.3..4.3).contains(&t) { 60.0 } else { 9.8 };
        d.rows.push(row(t, "ACCEL_DATA", "FC", &[0.0, 0.0, a]));
        if ((t * 20.0).round() as i64) % 20 == 0 {
            let lat = if t > f.land_t {
                43.01
            } else {
                43.0 + 0.01 * ((t - f.apogee_t) / (f.land_t - f.apogee_t)).clamp(0.0, 1.0)
            };
            d.rows.push(row(t, "GPS_DATA", "FC", &[lat, -78.8, 200.0 + h]));
        }
        t += 0.05;
    }
    for (t, phase) in [
        (0.0, 7.0),
        (0.3, 8.0),
        (4.3, 9.0),
        (f.apogee_t, 10.0),
        (f.drogue_t, 11.0),
        (f.main_t, 12.0),
        (f.land_t + 2.0, 13.0),
    ] {
        d.rows.push(row(t, "GS_FC_PHASE", "GS", &[phase]));
    }
    d.messages.push((ms(f.drogue_t + 0.05), "FC: PD approx altitude: 2430".into()));
    d.messages.push((ms(f.drogue_t + 0.5), "EV apogee guess: 2441".into()));
    d.messages.push((
        ms(f.main_t + 0.05),
        "Main parachute deployed at 300 m (FC), 12:00:00 UTC, T+111.2 s".into(),
    ));
    d.sort();
    d
}

#[test]
fn flight_summary_from_fc_phases_messages_and_telemetry() {
    let f = profile();
    let s = compute(&flight_data(&f), Some("normal"), Some("hardware")).expect("a flight");
    assert!(!s.simulated, "{}", s.data_label);
    assert_eq!((s.launch_accepted_ms, s.t0_ms), (Some(ms(-3.0)), Some(ms(0.0))));
    assert_eq!(s.t0_source, "main valve open");
    assert_eq!(s.liftoff_ms, Some(ms(0.3)), "{}", s.liftoff_source);
    let apogee = s.apogee.clone().unwrap();
    assert!((apogee.value - f.apogee_h).abs() < 3.0, "{apogee:?}");
    assert!(apogee.source.starts_with("FC Kalman altitude"));
    assert!(
        (s.time_to_apogee_s.unwrap() - (f.apogee_t - 0.3)).abs() < 0.2,
        "{:?}",
        s.time_to_apogee_s
    );
    assert_eq!(s.time_to_apogee_from, "liftoff");
    assert_eq!(s.fc_reported_apogee_m, Some(2441.0));
    assert_eq!(s.altitude_maxima.len(), 3, "FC, baro and GPS above the pad");
    let gps = s.altitude_maxima.iter().find(|p| p.source.starts_with("GPS")).unwrap();
    assert!((gps.value - f.apogee_h).abs() < 15.0, "GPS is above the pad: {gps:?}");
    let drogue = s.drogue.clone().unwrap();
    assert_eq!(drogue.t_ms, ms(f.drogue_t), "the phase edge is earlier than the message");
    assert_eq!(drogue.evidence, "FC phase Descent");
    assert_eq!((drogue.altitude_m, drogue.altitude_source.as_str()), (Some(2430.0), "FC reported"));
    let main = s.main.clone().unwrap();
    assert_eq!(main.t_ms, ms(f.main_t));
    assert_eq!(main.altitude_m, Some(300.0));
    assert_eq!(s.landed_ms, Some(ms(f.land_t + 2.0)));
    assert!((s.flight_time_s.unwrap() - (f.land_t + 2.0 - 0.3)).abs() < 0.1);
    assert!(
        (s.descent_rate_drogue_mps.unwrap() - 25.0).abs() < 0.5,
        "{:?}",
        s.descent_rate_drogue_mps
    );
    assert!(
        (s.descent_rate_main_mps.unwrap() - 6.0).abs() < 0.3,
        "{:?}",
        s.descent_rate_main_mps
    );
    let v = s.max_velocity.clone().unwrap();
    assert!((v.value - 200.0).abs() < 3.0, "{v:?}");
    let mach = 200.0 / speed_of_sound(200.0 + 400.0);
    assert!((s.max_mach.unwrap() - mach).abs() < 0.02, "{:?} vs {mach}", s.max_mach);
    assert!((s.max_acceleration_g.unwrap() - 60.0 / 9.80665).abs() < 0.05);
    assert!((s.landing_distance_m.unwrap() - 1112.0).abs() < 10.0, "{:?}", s.landing_distance_m);
    assert!(s.landing_bearing_deg.unwrap() < 1.0 || s.landing_bearing_deg.unwrap() > 359.0);
    assert_eq!(s.latest_phase.as_deref(), Some("Landed"));
    // Events: ordered, relative to T-0 and to Launch acceptance.
    let names: Vec<&str> = s.events.iter().map(|e| e.name.as_str()).collect();
    for expected in [
        "Launch accepted",
        "T-0",
        "Liftoff",
        "Apogee (max altitude)",
        "FC apogee detected",
        "Drogue deploy",
        "Main/reefing deploy",
        "Touchdown / Landed",
    ] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }
    assert!(s.events.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
    let drogue_event = s.events.iter().find(|e| e.name == "Drogue deploy").unwrap();
    assert!((drogue_event.t_plus_s.unwrap() - f.drogue_t).abs() < 0.01);
    assert!((drogue_event.t_launch_s.unwrap() - (f.drogue_t + 3.0)).abs() < 0.01);
    assert!(!s.altitude_curve.is_empty() && s.altitude_curve.len() <= 1501);
    assert!(!s.velocity_curve.is_empty());
}

#[test]
fn flight_summary_detects_landing_without_a_landed_report() {
    let f = profile();
    let mut d = flight_data(&f);
    d.rows.retain(|r| !(r.data_type == "GS_FC_PHASE" && r.v(0) == Some(13.0)));
    let s = compute(&d, None, None).unwrap();
    let landed = s.landed_ms.unwrap();
    assert!((landed - ms(f.land_t)).abs() < 1_500, "{} vs {}", landed, ms(f.land_t));
    assert!(s.landed_source.contains("steady near the ground"), "{}", s.landed_source);
}

#[test]
fn igt_summary_uses_only_the_replayed_flight_and_is_simulated() {
    let f = profile();
    let mut d = Data::default();
    d.rows.push(Row {
        t_ms: ms(-60.0),
        data_type: "GS_STATION_MODE".into(),
        sender: "GS".into(),
        values: vec![],
        payload: Some(r#"{"mode":"integrated_ground_test","igt_active":true}"#.into()),
    });
    d.rows.push(row(0.0, "VALVE_STATE", "VB", &[1.0]));
    d.rows.push(row(0.0, "FLIGHT_STATE", "FC", &[7.0]));
    d.rows.push(row(0.5, "FLIGHT_STATE", "FC", &[16.0]));
    let mut t = 0.0;
    while t < f.land_t + 15.0 {
        let (h, v) = height(&f, t);
        // The stand's real barometer and accelerometer stay on the ground.
        d.rows.push(row(t, "BAROMETER_DATA", "FC", &[0.4, 101_325.0, 20.0]));
        d.rows.push(row(t, "ACCEL_DATA", "FC", &[0.0, 0.0, 30.0]));
        let kind = if t < f.apogee_t { 4.0 } else { 5.0 };
        let values = if kind == 4.0 {
            [kind, t, 1.0, 0.0, 0.0, 0.0, h, v]
        } else {
            [kind, t, -78.8, 43.0, h, v, 0.0, 0.0]
        };
        d.rows.push(row(t, "SIM_SAMPLE", "FC", &values));
        t += 0.1;
    }
    d.messages.push((
        ms(f.drogue_t),
        "SIMULATED flight: drogue DRY-RUN (not fired) at 2430 m, 12:00:25 UTC, T+25.7 s".into(),
    ));
    d.messages.push((
        ms(f.main_t),
        "SIMULATED flight: main DRY-RUN (not fired) at 300 m, 12:01:50 UTC, T+111.2 s".into(),
    ));
    d.sort();
    let s = compute(&d, Some("integrated_ground_test"), Some("hardware")).unwrap();
    assert!(s.simulated && s.data_label.starts_with("SIMULATED flight"), "{}", s.data_label);
    assert!((s.apogee.as_ref().unwrap().value - f.apogee_h).abs() < 5.0, "{:?}", s.apogee);
    assert!(
        s.altitude_maxima.iter().all(|p| p.source.contains("replayed")),
        "{:?}",
        s.altitude_maxima
    );
    assert!(
        s.max_acceleration.is_none(),
        "the stand's accelerometer never feeds the replayed flight"
    );
    let drogue = s.drogue.unwrap();
    assert!(drogue.dry_run && drogue.altitude_m == Some(2430.0), "{drogue:?}");
    assert!(s.main.unwrap().dry_run);
    assert!(s.events.iter().any(|e| e.name == "Drogue deploy (DRY-RUN, not fired)"));
    assert!(s.notes.iter().any(|n| n.starts_with("Replayed flight")));
    assert_eq!(s.launch_accepted_ms, Some(ms(0.0)));
}

#[test]
fn no_flight_data_means_no_summary_and_messages_parse() {
    let mut d = Data::default();
    d.rows.push(row(0.0, "LOADCELL_WEIGHT_KG", "DAQ", &[1.0]));
    d.rows.push(row(0.0, "BAROMETER_DATA", "FC", &[0.0, 101_325.0, 20.0]));
    // A barometer on the pad is a (flat) flight review: nothing flew.
    let s = compute(&d, None, None).unwrap();
    assert!(s.apogee.is_some_and(|a| a.value == 0.0) && s.drogue.is_none() && s.landed_ms.is_none() && s.liftoff_ms.is_none());
    d.rows.retain(|r| r.data_type != "BAROMETER_DATA");
    assert!(compute(&d, None, None).is_none());
    assert_eq!(
        parse_deployment("FC: PR approx altitude: 304").map(|p| (p.0, p.1)),
        Some((Chute::Main, Some(304.0)))
    );
    assert_eq!(
        parse_deployment("Drogue parachute deployed at unknown altitude, 00:00:00 UTC").map(|p| (p.0, p.1)),
        Some((Chute::Drogue, None))
    );
    assert_eq!(
        parse_deployment("Drogue parachute deployed at 812 m (latest telemetry), 00:00:00 UTC").map(|p| p.1),
        Some(Some(812.0))
    );
    assert_eq!(
        parse_deployment("SIMULATED flight: LIVE main fired at 250 m, 00:00:00 UTC").map(|p| (p.0, p.1, p.3)),
        Some((Chute::Main, Some(250.0), false))
    );
    assert!(parse_deployment("PD drogue blocked, state: 3").is_none());
    let (d, b) = distance_bearing((43.0, -78.8), (43.0, -78.79));
    assert!((d - 813.0).abs() < 5.0 && (b - 90.0).abs() < 0.5, "{d} {b}");
}

#[test]
fn sections_follow_the_mode() {
    assert_eq!(sections("test_fire", false), (true, false));
    assert_eq!(sections("normal", false), (false, true));
    assert_eq!(sections("hitl", false), (true, true));
    assert_eq!(sections("integrated_ground_test", false), (true, true));
    assert_eq!(sections("test_fire", true), (true, true), "firmware simulation adds the flight");
}

#[test]
fn live_log_and_cache_feed_the_live_summary() {
    let f = profile();
    let rows: Vec<crate::types::TelemetryRow> = flight_data(&f)
        .rows
        .iter()
        .filter(|r| r.data_type != "GS_FC_PHASE")
        .map(|r| crate::types::TelemetryRow {
            timestamp_ms: r.t_ms,
            data_type: r.data_type.clone(),
            sender_id: r.sender.clone(),
            values: r.values.iter().map(|v| v.map(|x| x as f32)).collect(), over_range: 0, over_range_low: 0,
        })
        .collect();
    let from = ms(-20.0) + 7_777;
    let mut d = data::from_cache(rows.iter(), from);
    assert!(d.rows.iter().all(|r| r.t_ms >= from));
    // A private log: the process-wide one also receives other tests' events.
    let log = data::LiveLog::new();
    log.note(from - 1, LiveEntry::Message("FC: PD approx altitude: 9999".into()));
    log.note(ms(f.drogue_t), LiveEntry::FcPhase(11));
    log.note(ms(f.drogue_t) + 1, LiveEntry::FcPhase(11));
    log.note(ms(f.drogue_t) + 5, LiveEntry::Message("FC: PD approx altitude: 2431".into()));
    log.add_to(&mut d, from);
    assert_eq!(
        d.rows
            .iter()
            .filter(|r| r.data_type == "GS_FC_PHASE" && r.t_ms >= ms(f.drogue_t) && r.t_ms <= ms(f.drogue_t) + 1)
            .count(),
        1,
        "states are logged on change"
    );
    let s = compute(&d, Some("normal"), None).unwrap();
    assert_eq!(s.drogue.unwrap().altitude_m, Some(2431.0));
}

#[test]
fn propellant_split_and_planner_prediction() {
    assert_eq!(
        crate::burn_report::split_propellant(5.65, Some(6.65)),
        (Some(5.65), Some(1.0), Some(5.65))
    );
    assert_eq!(
        crate::burn_report::split_propellant(5.65, Some(5.6)),
        (Some(5.65), None, None),
        "no fuel left over"
    );
    assert_eq!(crate::burn_report::split_propellant(0.0, Some(5.6)), (None, None, None));
    let p = motor::planner_prediction(
        &serde_json::json!({"plan_id": "p1", "impulse_ns": 9192.0, "impulse_sigma_ns": 700.0, "fill_mass_kg": 5.65}),
    )
    .unwrap();
    let area: f64 = p.curve.windows(2).map(|w| (w[1][0] - w[0][0]) * (w[0][1] + w[1][1]) / 2.0).sum();
    assert!((area - 9192.0).abs() / 9192.0 < 0.01, "{area}");
    assert_eq!(p.model, "apogee_planner");
    assert!(motor::planner_prediction(&serde_json::json!({})).is_none());
}

#[test]
fn the_hybrid_model_prediction_is_preferred_and_carries_the_pressures() {
    let seed = crate::burn_report::tests::real_fire_report();
    let p = motor::hybrid_model_prediction(&seed).unwrap();
    assert_eq!(p.model, "hybrid_model");
    assert!((p.total_impulse_ns.unwrap() / 9192.0 - 1.0).abs() < 0.08, "{:?}", p.total_impulse_ns);
    assert_eq!(p.tank_psi.len(), p.curve.len());
    assert!(p.tank_psi[0][1] > 380.0);
    let chosen = motor::prediction(Some(&seed), Some(&serde_json::json!({"plan_id": "p1", "impulse_ns": 9192.0}))).unwrap();
    assert_eq!(chosen.model, "hybrid_model", "preferred over the planner's scaled curve");
}

#[test]
fn zip_round_trips() {
    let mut z = zip::Zip::new(T0);
    z.add("a.txt", b"hello hello hello hello hello");
    z.add("dir/b.bin", &[0u8, 1, 2]);
    let files = zip::read(&z.finish());
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], ("a.txt".into(), b"hello hello hello hello hello".to_vec()));
    assert_eq!(files[1].1, vec![0, 1, 2]);
}

async fn open_ro(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(path).read_only(true))
        .await
        .unwrap()
}

fn file<'a>(files: &'a [(String, Vec<u8>)], name: &str) -> &'a str {
    let bytes = &files.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("{name} missing")).1;
    std::str::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn test_fire_capture_review_and_motor_bundle() {
    let root = temp_root("review-motor");
    let name = "2027-01-15_08-00-00_launch";
    let folder = capture_folder(&root, name, "test_fire", true).await;
    let id = crate::recording_store::RecordingId::event_id(EventKind::Launch, name);
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0 + 100_000).await.unwrap();
    pool.close().await;
    let review = &loaded.review;
    assert_eq!(review.source_kind, "capture");
    assert!(!review.simulated);
    assert!(review.flight.is_none(), "a stand burn has no flight");
    let motor = review.motor.clone().expect("motor view");
    assert_eq!(motor.report_origin, "computed", "no report saved yet");
    let report = motor.report.clone().unwrap();
    assert!(report.coverage.daq_sd, "the attached DAQ launch file is merged");
    let truth = 2200.0 * 4.9;
    assert!(
        (report.total_impulse_ns - truth).abs() / truth < 0.05,
        "{}",
        report.total_impulse_ns
    );
    assert_eq!(report.tank_psi_at_ignition, Some(400.0));
    assert_eq!(report.tank_psi_end, Some(400.0));
    assert!(report.burn_end_s > 4.0);
    assert!(!motor.series.kg1000_kg.is_empty() && !motor.series.kg50_kg.is_empty() && !motor.series.tank_psi.is_empty());
    assert_eq!(motor.series.sources.get("kg1000").map(String::as_str), Some("mixed"));
    assert_eq!(motor.window_from_ms, Some(report.ignition_ms - 5_000));
    // The bundle: default range is the burn window.
    let (zip_name, bytes) = motor_export_bytes(loaded, T0 + 100_000, false).await.unwrap();
    assert_eq!(zip_name, "motor-launch.zip");
    let files = zip::read(&bytes);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [
        "motor_summary.json",
        "motor_summary.txt",
        "thrust_curve.csv",
        "loadcells_calibrated.csv",
        "loadcells_raw.csv",
        "chamber_pt.csv",
        "chamber_pt_raw.csv",
        "tank_pt.csv",
        "tank_pt_raw.csv",
        "temperatures.csv",
        "valve_events.csv",
        "daq_sd.csv",
        "plots/thrust.svg",
        "plots/chamber_pressure.svg",
        "plots/tank_pressure.svg",
        "plots/mass.svg",
        "report.html",
        "provenance.json",
        "README.txt",
    ] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }
    let json: serde_json::Value = serde_json::from_str(file(&files, "motor_summary.json")).unwrap();
    assert_eq!(json["format"], "seds-gs-motor-export");
    assert_eq!(json["motor"]["report"]["total_impulse_ns"].as_f64(), Some(report.total_impulse_ns));
    assert_eq!(json["provenance"]["source_id"], id);
    assert_eq!(json["provenance"]["from_ms"].as_i64(), motor.window_from_ms);
    let thrust = file(&files, "thrust_curve.csv");
    assert_eq!(thrust.lines().filter(|l| !l.starts_with('#')).count(), report.curve.len() + 1);
    let cells = file(&files, "loadcells_calibrated.csv");
    assert!(cells.contains(",kg1000,") && cells.contains(",daq_sd") && cells.contains(",gs_telemetry"));
    assert!(
        cells.lines().filter(|l| !l.starts_with('#')).skip(1).all(|l| {
            let t: i64 = l.split(',').next().unwrap().parse().unwrap();
            t >= motor.window_from_ms.unwrap() && t <= motor.window_to_ms.unwrap()
        }),
        "clipped to the burn window"
    );
    assert!(file(&files, "tank_pt.csv").contains(",tank,400,gs_telemetry"));
    assert!(file(&files, "valve_events.csv").contains(",VB,pilot,on/open"));
    assert!(file(&files, "plots/thrust.svg").starts_with("<svg") && file(&files, "plots/thrust.svg").contains("measured"));
    let html = file(&files, "report.html");
    assert!(html.contains("Total impulse") && html.contains("<svg") && !html.contains("SIMULATED DATA"));
    assert!(file(&files, "motor_summary.txt").contains("Total impulse"));
    assert!(file(&files, "README.txt").contains("thrust_curve.csv"));
    // A saved report is used as it is unless a range is chosen.
    crate::burn_report::recompute_folder_in(&root, &folder, None).await.unwrap();
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let saved = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    assert_eq!(saved.review.motor.as_ref().unwrap().report_origin, "saved");
    let ranged = load(
        &root,
        &id,
        &pool,
        &Range {
            from_ms: Some(T0 - 3_000),
            to_ms: Some(T0 + 15_000),
            ..Range::default()
        },
        T0,
    )
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(ranged.review.motor.as_ref().unwrap().report_origin, "computed");
    let _ = std::fs::remove_dir_all(&root);
}

/// The sample flight (`profile`) as an event capture under `root`, for
/// other modules' fixtures.
pub(crate) async fn sample_flight_capture(root: &std::path::Path, name: &str, mode: &str) -> std::path::PathBuf {
    flight_capture(root, name, mode, &profile()).await
}

async fn flight_capture(root: &std::path::Path, name: &str, mode: &str, f: &Flight) -> std::path::PathBuf {
    let folder = crate::recording_store::events_dir(root).join("launch").join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let (db, _) = crate::telemetry_db::open_telemetry_db(&folder.join(crate::recording_store::CAPTURE_FILE))
        .await
        .unwrap();
    let d = flight_data(f);
    sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,'GS_STATION_MODE','GS',NULL,?)")
        .bind(ms(-100.0))
        .bind(ms(-100.0))
        .bind(format!(r#"{{"mode":"{mode}","igt_active":false,"provenance":"hardware"}}"#))
        .execute(&db)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    for r in &d.rows {
        let values: Vec<f64> = r.values.iter().map(|v| v.unwrap()).collect();
        sqlx::query(
            "INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(?,?,?,?,?,'[]')",
        )
        .bind(r.t_ms + 30)
        .bind(r.t_ms)
        .bind(&r.data_type)
        .bind(&r.sender)
        .bind(serde_json::to_string(&values).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    for (i, (t, m)) in d.messages.iter().enumerate() {
        sqlx::query("INSERT INTO messages(id,timestamp_ms,message) VALUES(?,?,?)")
            .bind(i as i64 + 1)
            .bind(t)
            .bind(m)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    for (t, c) in &d.station_states {
        sqlx::query("INSERT INTO flight_state(timestamp_ms,f_state) VALUES(?,?)")
            .bind(t)
            .bind(*c as i64)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    db.close().await;
    let m = manifest(name, mode, serde_json::json!({"trigger_flight_state": 7}));
    std::fs::write(
        folder.join(crate::recording_store::MANIFEST_FILE),
        serde_json::to_vec_pretty(&m).unwrap(),
    )
    .unwrap();
    folder
}

#[tokio::test]
async fn flight_capture_review_and_flight_bundle() {
    let root = temp_root("review-flight");
    let f = profile();
    let name = "2027-02-01_10-00-00_launch";
    let folder = flight_capture(&root, name, "normal", &f).await;
    let id = crate::recording_store::RecordingId::event_id(EventKind::Launch, name);
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    pool.close().await;
    assert_eq!(loaded.review.mode.as_deref(), Some("normal"));
    assert!(
        loaded.review.motor.is_none(),
        "a flight capture without load cells has no motor section"
    );
    let s = loaded.review.flight.clone().expect("flight summary");
    assert!((s.apogee.as_ref().unwrap().value - f.apogee_h).abs() < 3.0);
    assert_eq!(
        s.drogue.as_ref().unwrap().altitude_m,
        Some(2430.0),
        "from the recording's message table"
    );
    assert_eq!(s.launch_accepted_ms, Some(ms(-3.0)), "from the recording's flight_state table");
    let (zip_name, bytes) = flight_export_bytes(loaded, T0, false).await.unwrap();
    assert_eq!(zip_name, "flight-launch.zip");
    let files = zip::read(&bytes);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    for expected in [
        "flight_summary.json",
        "flight_summary.txt",
        "events.csv",
        "altitude.csv",
        "velocity.csv",
        "acceleration.csv",
        "gps_track.csv",
        "plots/altitude.svg",
        "plots/velocity.svg",
        "report.html",
        "README.txt",
        "provenance.json",
    ] {
        assert!(names.contains(&expected), "{expected} missing from {names:?}");
    }
    let events = file(&files, "events.csv");
    assert!(events.starts_with("event,network_ms,utc,t_plus_s,t_from_launch_s,altitude_m,source"));
    assert!(events.contains("Apogee (max altitude)") && events.contains("Drogue deploy") && events.contains("Touchdown / Landed"));
    assert!(file(&files, "altitude.csv").contains("FC Kalman altitude"));
    let json: serde_json::Value = serde_json::from_str(file(&files, "flight_summary.json")).unwrap();
    assert_eq!(json["flight"]["drogue"]["altitude_m"].as_f64(), Some(2430.0));
    assert!(json["provenance"]["from_ms"].as_i64() == Some(ms(-33.0)), "{}", json["provenance"]);
    assert!(file(&files, "plots/altitude.svg").contains("Apogee"));
    assert!(file(&files, "report.html").contains("Events") && file(&files, "flight_summary.txt").contains("Apogee"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn hitl_capture_review_is_simulated_with_both_sections() {
    let root = temp_root("review-hitl");
    let name = "2027-01-16_08-00-00_launch";
    let folder = capture_folder(&root, name, "hitl", false).await;
    let id = crate::recording_store::RecordingId::event_id(EventKind::Launch, name);
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    pool.close().await;
    assert!(loaded.review.simulated);
    let motor = loaded.review.motor.clone().unwrap();
    assert!(motor.simulated && motor.report.unwrap().simulated);
    let (_, bytes) = motor_export_bytes(loaded, T0, false).await.unwrap();
    let files = zip::read(&bytes);
    assert!(file(&files, "report.html").contains("SIMULATED DATA"));
    assert!(file(&files, "README.txt").contains("SIMULATED DATA"));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fire_archive_seed_without_a_capture_exports_its_curve() {
    let root = temp_root("review-fire");
    crate::fire_archive::ensure_seeded(&root).unwrap();
    let fires = crate::fire_archive::load_all(&root);
    let Some(seed) = fires
        .iter()
        .find(|r| r.source == "seed" && crate::fire_archive::curve(&root, &r.id).is_some_and(|c| !c.is_empty()))
    else {
        panic!(
            "a seeded fire with a curve: {:?}",
            fires.iter().map(|r| (&r.id, &r.source)).collect::<Vec<_>>()
        );
    };
    let id = format!("fire.{}", seed.id);
    let pool = crate::recording_replay::sources::tests_open(&root, &id).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    let motor = loaded.review.motor.clone().unwrap();
    assert_eq!(motor.report_origin, "saved");
    let report = motor.report.unwrap();
    assert_eq!(report.total_impulse_ns, seed.impulse_ns.unwrap());
    assert!(motor.archive.is_some());
    let (_, bytes) = motor_export_bytes(loaded, T0, false).await.unwrap();
    let files = zip::read(&bytes);
    assert!(file(&files, "thrust_curve.csv").lines().count() > 10);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn routes_need_view_data_and_review_any_source() {
    use crate::fire_archive::tests::{call, http_state};
    let state = http_state().await;
    crate::fire_archive::ensure_seeded(&crate::fire_archive::data_dir(&state)).unwrap();
    for uri in [
        "/api/flight_review/live",
        "/api/flight_review/recording/fire.2026-11-16_static_fire",
        "/api/flight_review/recording/fire.2026-11-16_static_fire/motor_export",
    ] {
        let (status, _) = call(&state, "GET", uri, None, "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, body) = call(&state, "GET", "/api/flight_review/live", Some("viewer-token"), "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["mode"].is_string() && body["show_motor"].is_boolean(), "{body}");
    let (status, body) = call(
        &state,
        "GET",
        "/api/flight_review/recording/fire.2026-11-16_static_fire",
        Some("viewer-token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source_kind"], "fire");
    assert!(
        body["motor"]["report"]["total_impulse_ns"].as_f64().is_some_and(|i| i > 8000.0),
        "{body}"
    );
    assert!(body["motor"]["archive"]["id"] == "2026-11-16_static_fire");
    let (status, _) = call(
        &state,
        "GET",
        "/api/flight_review/recording/event.launch.missing.db/flight_export",
        Some("viewer-token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(&state, "GET", "/api/flight_review/recording/..%2Fetc", Some("viewer-token"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &state,
        "GET",
        "/api/flight_review/recording/fire.2026-11-16_static_fire?from_ms=5&to_ms=4",
        Some("viewer-token"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[test]
fn a_burn_in_progress_shows_the_raw_channels_before_its_report() {
    let mut d = Data::default();
    d.rows.push(row(0.0, "VALVE_STATE", "VB", &[1.0]));
    for i in -40..40 {
        let t = i as f64 * 0.05;
        d.rows.push(row(t, "LOADCELL_50_WEIGHT_KG", "DAQ", &[40.0]));
        d.rows
            .push(row(t, "COMBUSTION_CHAMBER_PRESSURE", "DAQ", &[if t > 0.2 { 300.0 } else { 0.0 }]));
    }
    let view = motor::view(
        &d,
        motor::Context {
            id: "live-1".into(),
            label: "Launch".into(),
            mode: "test_fire".into(),
            simulated: false,
            created_ms: 0,
            saved: None,
            plan: None,
            extra_daq: vec![],
            archive: None,
        },
    )
    .unwrap();
    assert!(view.report.is_none() && view.reason.is_some(), "{view:?}");
    assert_eq!(view.ignition_ms, Some(T0));
    assert!(view.series.chamber_psi.iter().any(|p| p[1] == 300.0) && !view.series.kg50_kg.is_empty());
    assert!(view.series.chamber_psi.first().unwrap()[0] < 0.0, "relative to the valve opening");
}

/// Writes sample bundles (with the PNG/PDF step) to `GS_REVIEW_SAMPLE_DIR`:
/// `cargo test -p groundstation_backend write_sample_bundles -- --ignored`.
#[tokio::test]
#[ignore]
async fn write_sample_bundles() {
    let Some(out) = std::env::var_os("GS_REVIEW_SAMPLE_DIR").map(std::path::PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&out).unwrap();
    let root = temp_root("review-samples");
    let name = "2027-01-15_08-00-00_launch";
    let folder = capture_folder(&root, name, "test_fire", true).await;
    let id = crate::recording_store::RecordingId::event_id(EventKind::Launch, name);
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    pool.close().await;
    std::fs::write(
        out.join("review-test-fire.json"),
        serde_json::to_vec_pretty(&loaded.review).unwrap(),
    )
    .unwrap();
    let (n, bytes) = motor_export_bytes(loaded, T0 + 100_000, true).await.unwrap();
    std::fs::write(out.join(n), bytes).unwrap();
    let f = profile();
    let name = "2027-02-01_10-00-00_launch";
    let folder = flight_capture(&root, name, "normal", &f).await;
    let id = crate::recording_store::RecordingId::event_id(EventKind::Launch, name);
    let pool = open_ro(&folder.join(crate::recording_store::CAPTURE_FILE)).await;
    let loaded = load(&root, &id, &pool, &Range::default(), T0).await.unwrap();
    pool.close().await;
    std::fs::write(out.join("review-flight.json"), serde_json::to_vec_pretty(&loaded.review).unwrap()).unwrap();
    let (n, bytes) = flight_export_bytes(loaded, T0 + 300_000, true).await.unwrap();
    std::fs::write(out.join(n), bytes).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
