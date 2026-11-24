use super::*;
use crate::apogee_planner::flight::{Config, Environment, Motor, THRUST_CSV, fly, fly_traced};

fn manual(wind: f64, from: f64) -> weather::ManualWeather {
    weather::ManualWeather { surface_temp_c: 20.0, wind_mps: wind, wind_from_deg: from, gust_mps: 1.0, ..Default::default() }
}

fn request(fill: f64, samples: u32) -> Request {
    Request {
        kind: Some("flight".into()),
        label: Some("test".into()),
        fill_kg: Some(fill),
        supercharge_psi: Some(750.0),
        tank_temp_c: Some(15.0),
        motor_model: Some("scaling".into()),
        weather: Some(WeatherRequest { source: Some("manual".into()), manual: Some(manual(4.0, 300.0)) }),
        site: Some(weather::Site { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.016 }),
        dispersion: Some(DispersionRequest { samples: Some(samples), seed: Some(7) }),
        ..Default::default()
    }
}

#[test]
fn a_trace_does_not_change_the_flight() {
    let cfg = Config::reference();
    let env = Environment::from_config(&cfg);
    let motor = Motor::from_config(&cfg.motor, THRUST_CSV);
    let plain = fly(&cfg, &env, &motor, 0.001);
    let mut trace = flight::Trace::new(0.05, 0.5);
    let traced = fly_traced(&cfg, &env, &motor, 0.001, Some(&mut trace));
    assert_eq!(plain, traced);
    // Phases in flight order, never back.
    let order = ["pad", "boost", "coast", "drogue", "main", "landed"];
    let mut last = 0;
    for p in &trace.points {
        let k = order.iter().position(|o| *o == p.phase).unwrap();
        assert!(k >= last, "{} after {}", p.phase, order[last]);
        last = k;
    }
    assert_eq!(last, 5);
    let kinds: Vec<&str> = trace.events.iter().map(|e| e.0).collect();
    assert_eq!(kinds, ["ignition", "liftoff", "rail_exit", "burnout", "apogee", "drogue", "main", "landing"]);
    let land = trace.events.last().unwrap();
    assert!((land.2[0].hypot(land.2[1]) - plain.drift_m).abs() < 1e-6);
    assert!((trace.events[4].2[2] - plain.apogee_m).abs() < 1e-9);
    // Attitude only in the ascent.
    assert!(trace.points.iter().filter(|p| p.phase == "drogue").all(|p| p.q.is_none()));
    assert!(trace.points.iter().filter(|p| p.phase == "boost").all(|p| p.q.is_some()));
}

#[test]
fn geometry_helpers() {
    let o = Origin { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.0 };
    let (lat, lon) = o.lat_lon(1000.0, -500.0);
    let (e, n) = o.en(lat, lon);
    assert!((e - 1000.0).abs() < 1e-6 && (n + 500.0).abs() < 1e-6);
    let (d, b) = distance_bearing(o.lat_deg, o.lon_deg, lat, lon);
    assert!((d - 1118.0).abs() < 2.0, "{d}");
    assert!((b - 116.6).abs() < 0.5, "{b}");
    // An ellipse along north-east.
    let el = Ellipse::from_covariance(0.0, 0.0, 50.0, 40.0, 50.0, 1.0);
    assert!((el.semi_major_m - 90f64.sqrt()).abs() < 1e-9 && (el.semi_minor_m - 10f64.sqrt()).abs() < 1e-9);
    assert!((el.major_bearing_deg - 45.0).abs() < 1e-9);
    let ring = el.polygon(&o, 36);
    assert_eq!(ring.len(), 36);
    // From samples: the deterministic normals have the requested spread.
    let mut rng = Rng::new(3);
    let pts: Vec<(f64, f64)> = (0..4000).map(|_| (rng.normal() * 30.0, rng.normal() * 10.0)).collect();
    let el = Ellipse::of(&pts, 1.0).unwrap();
    assert!((el.semi_major_m - 30.0).abs() < 1.5 && (el.semi_minor_m - 10.0).abs() < 0.6, "{el:?}");
    assert!((el.major_bearing_deg - 90.0).abs() < 2.0, "{el:?}");
    assert!(Ellipse::of(&pts[..2], 1.0).is_none());
}

#[test]
fn wind_rotation_turns_the_profile() {
    let mut w = flight::WindProfile::power_law(5.0, 270.0, 0.14, 1.4);
    w.levels = vec![(1000.0, 5.0, 0.0)];
    let r = w.rotated(90.0);
    assert_eq!(r.from_deg, 0.0);
    // Wind from the west (blowing east) turned 90° clockwise blows south.
    assert!((r.levels[0].1).abs() < 1e-9 && (r.levels[0].2 + 5.0).abs() < 1e-9);
    let (e, n) = r.at(5.0);
    assert!(e.abs() < 1e-9 && n < 0.0);
}

#[test]
fn phase_tracks_join_without_gaps() {
    let path: Vec<(String, [f64; 3])> = ["pad", "boost", "boost", "coast", "coast", "drogue"].iter().enumerate().map(|(i, p)| (p.to_string(), [i as f64, 0.0, 0.0])).collect();
    let tracks = phase_tracks(&path, "predicted");
    let phases: Vec<&str> = tracks.iter().map(|t| t["phase"].as_str().unwrap()).collect();
    // The single pad point joins the first boost point; the last drogue point stands alone and is dropped.
    assert_eq!(phases, ["pad", "boost", "coast"]);
    assert_eq!(tracks[1]["points"].as_array().unwrap().len(), 3);
    assert_eq!(tracks[1]["points"][2][0], 3.0, "boost ends where coast starts");
}

#[test]
fn csv_names_events_on_their_rows() {
    let plan = json!({
        "kind": "flight",
        "events": [{"kind": "ignition", "t_s": 0.0}, {"kind": "liftoff", "t_s": 0.12}],
        "points": [
            {"t_s": 0.0, "phase": "pad", "e_m": 0, "n_m": 0, "u_m": 0, "lat_deg": 1.0, "lon_deg": 2.0, "alt_msl_m": 3.0, "ve_mps": 0, "vn_mps": 0, "vu_mps": 0, "attitude_q": [1, 0, 0, 0]},
            {"t_s": 0.15, "phase": "boost", "e_m": 0, "n_m": 0, "u_m": 0.1, "lat_deg": 1.0, "lon_deg": 2.0, "alt_msl_m": 3.1, "ve_mps": 0, "vn_mps": 0, "vu_mps": 1.5, "attitude_q": null},
        ],
    });
    let text = csv(&plan);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("t_s,phase,event,east_m"));
    assert_eq!(lines[1], "0,pad,ignition,0,0,0,1,2,3,0,0,0,1,0,0,0");
    assert_eq!(lines[2], "0.15,boost,liftoff,0,0,0.1,1,2,3.1,0,0,1.5,,,,");
}

#[test]
fn requests_are_validated() {
    assert!(request(8.0, 10).validate().is_ok());
    for bad in [
        Request { fill_kg: Some(-1.0), ..Default::default() },
        Request { kind: Some("orbit".into()), ..Default::default() },
        Request { dispersion: Some(DispersionRequest { samples: Some(MAX_SAMPLES + 1), seed: None }), ..Default::default() },
        Request { launch: Some(LaunchRequest { tilt_deg: Some(45.0), ..Default::default() }), ..Default::default() },
        Request { label: Some("x".repeat(81)), ..Default::default() },
        Request { tank_temp_c: Some(f64::NAN), ..Default::default() },
    ] {
        assert_eq!(bad.validate().unwrap_err().0, StatusCode::BAD_REQUEST, "{bad:?}");
    }
}

async fn station() -> (crate::recording_export::tests::TestDirectory, Arc<AppState>) {
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    (root, state)
}

#[tokio::test]
async fn a_plan_flies_the_planners_models_and_stores() {
    let (_root, state) = station().await;
    let req = request(8.5, 40);
    let started = std::time::Instant::now();
    let plan = compute(&state, &req, Some("op".into()), 1_767_225_600_000).unwrap();
    let wall = started.elapsed();
    assert_eq!(plan["format"], FORMAT);
    assert_eq!(plan["version"], VERSION);
    assert_eq!(plan["predicted"], true);
    // The same fill through the planner's own solver input flies the same flight.
    let r = resolve(&state, &req, 1_767_225_600_000).unwrap();
    let i = &r.prepared.input;
    let m = motor::motor_for_fill(&i.cfg, &i.fit, 8.5, i.tank_c, 750.0, i.tank_volume_l, 1.0);
    let env = i.weather.environment(i.cfg.environment.wind_shear_exponent, i.cfg.environment.wind_profile_cap);
    let direct = flight::fly(&i.cfg, &env, &m.motor, i.dt);
    assert_eq!(plan["summary"]["apogee_m"].as_f64().unwrap(), round(direct.apogee_m, 1));
    assert_eq!(plan["summary"]["drift_m"].as_f64().unwrap(), round(direct.drift_m, 1));
    let points = plan["points"].as_array().unwrap();
    assert!(points.len() > 300, "{}", points.len());
    assert_eq!(points.last().unwrap()["phase"], "landed");
    assert_eq!(plan["landing"]["alt_msl_m"].as_f64().unwrap(), 890.02);
    let d = &plan["dispersion"];
    assert_eq!(d["samples"], 40);
    assert_eq!(d["landing_points"].as_array().unwrap().len(), 40);
    let el = &d["landing_ellipse"];
    assert!(el["semi_major_m"].as_f64().unwrap() > el["semi_minor_m"].as_f64().unwrap());
    assert_eq!(el["polygon"].as_array().unwrap().len(), 48);
    let a = &d["apogee_m"];
    assert!(a["p05"].as_f64().unwrap() < a["mean"].as_f64().unwrap() && a["mean"].as_f64().unwrap() < a["p95"].as_f64().unwrap());
    // The overlay: phase tracks, markers, the ellipse.
    let ov = &plan["overlay"];
    assert_eq!(ov["kind"], "flight_plan");
    let phases: Vec<&str> = ov["tracks"].as_array().unwrap().iter().map(|t| t["phase"].as_str().unwrap()).collect();
    assert_eq!(phases, ["pad", "boost", "coast", "drogue", "main"]);
    assert_eq!(ov["areas"][0]["kind"], "landing_ellipse");
    assert!(ov["markers"].as_array().unwrap().iter().any(|m| m["kind"] == "landing"));
    // Determinism: the same request and time give the same document (bar the timings).
    let mut again = compute(&state, &req, Some("op".into()), 1_767_225_600_000).unwrap();
    let mut first = plan.clone();
    for p in [&mut first, &mut again] {
        p["compute_ms"] = json!(0);
        p["dispersion"]["compute_ms"] = json!(0);
    }
    assert_eq!(first, again);
    println!("plan with 40 samples: {wall:?} (debug build)");

    // Storage.
    let dir = directory(&state);
    save(&dir, &plan).unwrap();
    let id = plan["id"].as_str().unwrap();
    assert!(valid_id(id));
    assert_eq!(load(&dir, id).unwrap(), plan);
    assert_eq!(list(&dir).len(), 1);
    let s = summary(&plan);
    assert_eq!(s["dispersion_samples"], 40);
    assert!(load(&dir, "../etc/passwd").is_none());

    // Replay document and bundle.
    let replay = replay_document(&plan);
    assert_eq!(replay["predicted"], true);
    let samples = replay["samples"].as_array().unwrap();
    let states: Vec<i64> = samples.iter().filter(|s| s["data_type"] == "FLIGHT_STATE").map(|s| s["values"][0].as_i64().unwrap()).collect();
    assert_eq!(states, [6, 8, 9, 11, 12, 13]);
    assert_eq!(samples[0]["timestamp_ms"], 1_767_225_600_000i64);
    let gps: Vec<&Value> = samples.iter().filter(|s| s["data_type"] == "GPS_DATA").collect();
    assert!(gps.len() > 200);
    assert!(replay_csv(&replay).starts_with("id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json\n"));
    let headers = HeaderMap::new();
    let bundle = crate::recording_bundle::build_plan(state.clone(), &headers, &plan, 1_767_225_600_000).await.unwrap();
    let again = crate::recording_bundle::build_plan(state.clone(), &headers, &plan, 1_767_225_600_000).await.unwrap();
    let (ea, eb) = (crate::flight_review::zip::read(&bundle), crate::flight_review::zip::read(&again));
    for ((na, a), (_, b)) in ea.iter().zip(eb.iter()) {
        assert!(a == b, "{na} differs:\n{}\n{}", String::from_utf8_lossy(&a[..a.len().min(3000)]), String::from_utf8_lossy(&b[..b.len().min(3000)]));
    }
    assert_eq!(bundle, again, "a plan bundle is reproducible");
    let names: Vec<String> = crate::flight_review::zip::read(&bundle).into_iter().map(|(n, _)| n).collect();
    for want in ["manifest.json", "replay.json", "data/telemetry.csv", "derived/flight_plan.json", "derived/trajectory.json"] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
}

#[tokio::test]
async fn a_target_apogee_uses_the_solver_and_a_burn_plan_has_no_trajectory() {
    let (_root, state) = station().await;
    let mut req = request(8.0, 0);
    req.fill_kg = None;
    req.target_apogee_m = Some(2500.0);
    let plan = compute(&state, &req, None, 1).unwrap();
    let apogee = plan["summary"]["apogee_m"].as_f64().unwrap();
    assert!((apogee - 2500.0).abs() < 60.0, "{apogee}");
    assert!(plan["dispersion"].is_null());
    let mut burn = request(8.0, 50);
    burn.kind = Some("burn".into());
    burn.motor_model = Some("physics".into());
    let plan = compute(&state, &burn, None, 2).unwrap();
    assert!(plan["points"].as_array().unwrap().is_empty());
    assert!(plan["overlay"].is_null() && plan["dispersion"].is_null());
    let b = &plan["burn"];
    let n = b["t_s"].as_array().unwrap().len();
    assert!(n > 50);
    for k in ["thrust_n", "chamber_psi", "tank_psi", "mdot_ox_kg_s", "mdot_fuel_kg_s", "of_ratio", "n2o_kg"] {
        assert_eq!(b[k].as_array().unwrap().len(), n, "{k}");
    }
    assert!(b["summary"]["impulse_ns"].as_f64().unwrap() > 1000.0);
    let text = csv(&plan);
    assert!(text.starts_with("t_s,thrust_n,chamber_psi,tank_psi"));
    assert_eq!(text.lines().count(), n + 1);
}

#[test]
fn storage_keeps_the_newest_plans() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let dir = root.0.join(DIR);
    for k in 0..(MAX_PLANS + 3) {
        save(&dir, &json!({"id": format!("fp-{}-00000000", 1000 + k), "label": "x"})).unwrap();
    }
    let ids: Vec<String> = list(&dir).iter().map(|p| p["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), MAX_PLANS);
    assert_eq!(ids[0], format!("fp-{}-00000000", 1000 + MAX_PLANS + 2), "newest first");
    assert!(!ids.contains(&"fp-1000-00000000".to_string()));
}

/// Model validation: the plan's flight against the nominal_10k mock flight
/// (tools/flight_profile, the Python model with gusts and parachute swing)
/// and the SWEET_V6 measured-motor case. Printed with --nocapture.
#[test]
fn validation_against_the_mock_flight() {
    let summary: Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../test-data/igt/nominal_10k/summary.json")).unwrap()).unwrap();
    let mut cfg = Config::reference();
    cfg.motor.total_impulse_ns = summary["motor"]["total_impulse_ns"].as_f64();
    cfg.motor.scale_propellant_with_impulse = true;
    let mut env = Environment::from_config(&cfg);
    env.site_msl_m = summary["site"][2].as_f64().unwrap();
    env.site_lat_deg = summary["site"][0].as_f64().unwrap();
    env.wind = flight::WindProfile::power_law(summary["wind"]["mps"].as_f64().unwrap(), summary["wind"]["from_deg"].as_f64().unwrap(), cfg.environment.wind_shear_exponent, cfg.environment.wind_profile_cap);
    let motor = Motor::from_config(&cfg.motor, THRUST_CSV);
    let mut tr = flight::Trace::new(0.05, 0.5);
    let r = fly_traced(&cfg, &env, &motor, 0.001, Some(&mut tr));
    let land = tr.events.iter().find(|e| e.0 == "landing").unwrap();
    let ref_apogee = summary["apogee_m_agl"].as_f64().unwrap();
    let ref_drift = summary["landing_drift_m"].as_f64().unwrap();
    let ref_bearing = summary["landing_bearing_deg"].as_f64().unwrap();
    let ref_land_s = summary["landing_s"].as_f64().unwrap() - summary["ignition_s"].as_f64().unwrap();
    println!(
        "nominal_10k: apogee {:.0} m (mock {ref_apogee:.0}, {:+.2} %), drift {:.0} m @ {:.1}° (mock {ref_drift:.0} m @ {ref_bearing:.1}°), landing {:.1} s (mock {ref_land_s:.1} s)",
        r.apogee_m,
        (r.apogee_m / ref_apogee - 1.0) * 100.0,
        r.drift_m,
        r.drift_bearing_deg,
        land.1
    );
    assert!((r.apogee_m / ref_apogee - 1.0).abs() < 0.02);
    let (re, rn) = (ref_drift * ref_bearing.to_radians().sin(), ref_drift * ref_bearing.to_radians().cos());
    let miss = (land.2[0] - re).hypot(land.2[1] - rn);
    println!("nominal_10k: landing point {miss:.0} m from the mock flight's");
    assert!(miss < 0.25 * ref_drift, "{miss}");
    assert!((land.1 / ref_land_s - 1.0).abs() < 0.1);
}
