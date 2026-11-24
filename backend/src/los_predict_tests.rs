use super::*;
use crate::apogee_planner::weather::{ManualWeather, Site};

const SITE: Site = Site { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.016 };

/// The nominal_10k mock flight's GPS track: (t s, lat, lon, alt MSL).
fn mock_gps() -> Vec<(f64, f64, f64, f64)> {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../test-data/igt/nominal_10k/IGT/GPS0.CSV"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let v: Vec<f64> = l.split(',').map(|x| x.parse().unwrap()).collect();
            (v[0] / 1000.0, v[1], v[2], v[3])
        })
        .collect()
}

fn mock_model() -> Model {
    // The mock flight's weather (summary.json): 4 m/s from 300°, 30 °C at the pad.
    let w = ManualWeather { surface_temp_c: 30.0, wind_mps: 4.0, wind_from_deg: 300.0, gust_mps: 1.0, ..Default::default() };
    Model::from_weather(&w.snapshot(SITE, 0, 0), SITE)
}

/// The mock flight's phase at t (summary.json events).
fn mock_phase(t: f64) -> Option<Phase> {
    match t {
        t if t < 19.395 => Some(Phase::Boost),
        t if t < 37.785 => Some(Phase::Coast),
        t if t < 141.272 => Some(Phase::Drogue),
        t if t < 204.03 => Some(Phase::Main),
        _ => None,
    }
}

fn fix_at(gps: &[(f64, f64, f64, f64)], t: f64, origin: &Origin) -> LastFix {
    let i = gps.iter().position(|g| g.0 >= t).unwrap();
    let j = gps.iter().position(|g| g.0 >= t - 2.0).unwrap();
    let (a, b) = (gps[i], gps[j]);
    let (e1, n1) = origin.en(a.1, a.2);
    let (e0, n0) = origin.en(b.1, b.2);
    let dt = a.0 - b.0;
    LastFix {
        t_ms: (a.0 * 1000.0) as i64,
        lat_deg: a.1,
        lon_deg: a.2,
        alt_msl_m: a.3,
        ve_mps: Some((e1 - e0) / dt),
        vn_mps: Some((n1 - n0) / dt),
        vu_mps: Some((a.3 - b.3) / dt),
        source: Some("FC GPS".into()),
    }
}

fn gps_at(gps: &[(f64, f64, f64, f64)], t: f64) -> (f64, f64, f64, f64) {
    *gps.iter().find(|g| g.0 >= t).unwrap_or(gps.last().unwrap())
}

/// Cuts the mock flight's signal at points along the flight and checks the
/// predicted landing against where it landed, and the predicted position 20 s
/// after the cut against the track. Printed with --nocapture.
#[test]
fn signal_cuts_on_the_mock_flight_predict_the_landing() {
    let gps = mock_gps();
    let m = mock_model();
    let land = gps_at(&gps, 206.0);
    println!("cut s | phase   | landing error m | 95% ellipse m | +20 s position error m | in 95% area");
    for (cut, max_err) in [(25.0, 400.0), (36.0, 300.0), (45.0, 250.0), (80.0, 200.0), (120.0, 150.0), (145.0, 80.0), (180.0, 60.0)] {
        let fix = fix_at(&gps, cut, &m.origin);
        let phase = mock_phase(cut);
        let started = std::time::Instant::now();
        let p = predict(&m, &fix, phase, 20.0, SAMPLES, 1);
        let cpu = started.elapsed();
        let (llat, llon) = m.origin.lat_lon(p.landing_en.0, p.landing_en.1);
        let (err, _) = distance_bearing(llat, llon, land.1, land.2);
        let (plat, plon) = m.origin.lat_lon(p.now_enu[0], p.now_enu[1]);
        let truth = gps_at(&gps, cut + 20.0);
        let (now_err, _) = distance_bearing(plat, plon, truth.1, truth.2);
        let el = p.landing_ellipse.unwrap();
        // Is the true landing inside the 95 % ellipse?
        let (te, tn) = m.origin.en(land.1, land.2);
        let b = el.major_bearing_deg.to_radians();
        let (de, dn) = (te - el.e, tn - el.n);
        let (x, y) = (de * b.sin() + dn * b.cos(), de * b.cos() - dn * b.sin());
        let inside = (x / el.semi_major_m).powi(2) + (y / el.semi_minor_m.max(0.5)).powi(2) <= 1.0;
        println!(
            "{cut:5.0} | {:7} | {err:15.0} | {:6.0} x {:4.0} | {now_err:22.0} | {inside}  ({cpu:?})",
            format!("{phase:?}").to_lowercase().replace("some(", "").replace(')', ""),
            el.semi_major_m,
            el.semi_minor_m
        );
        assert!(err < max_err, "cut at {cut} s: landing error {err:.0} m");
        assert!(now_err < 150.0, "cut at {cut} s: position error {now_err:.0} m 20 s later");
    }
}

#[test]
fn uncertainty_grows_with_time_since_the_fix() {
    let gps = mock_gps();
    let m = mock_model();
    let fix = fix_at(&gps, 60.0, &m.origin);
    let a = predict(&m, &fix, Some(Phase::Drogue), 5.0, SAMPLES, 1).now_ellipse.unwrap();
    let b = predict(&m, &fix, Some(Phase::Drogue), 60.0, SAMPLES, 1).now_ellipse.unwrap();
    assert!(b.semi_major_m > 2.0 * a.semi_major_m, "{a:?} {b:?}");
    // After the landing the position stops at the landing point.
    let p = predict(&m, &fix, Some(Phase::Drogue), 2000.0, 0, 1);
    assert_eq!(p.phase_now, "landed");
    assert_eq!(p.landing_in_s, 0.0);
    assert!(((p.now_enu[0], p.now_enu[1]).0 - p.landing_en.0).abs() < 1e-9);
}

#[test]
fn a_climbing_fix_coasts_to_apogee_first() {
    let m = mock_model();
    let fix = LastFix { t_ms: 0, lat_deg: SITE.lat_deg, lon_deg: SITE.lon_deg, alt_msl_m: SITE.alt_msl_m + 1500.0, ve_mps: Some(0.0), vn_mps: Some(0.0), vu_mps: Some(150.0), source: None };
    let p = predict(&m, &fix, Some(Phase::Coast), 5.0, 0, 1);
    assert!(p.now_enu[2] > 1500.0 + 400.0, "still climbing: {:?}", p.now_enu);
    let top = p.path.iter().map(|x| x.1[2]).fold(0.0, f64::max);
    assert!(top > 2300.0 && top < 3200.0, "{top}");
    assert_eq!(p.path.first().unwrap().0, "coast");
    assert!(p.path.iter().any(|x| x.0 == "drogue") && p.path.iter().any(|x| x.0 == "main"));
}

#[test]
fn the_route_view_has_the_overlay() {
    let gps = mock_gps();
    let m = mock_model();
    let fix = fix_at(&gps, 100.0, &m.origin);
    let p = predict(&m, &fix, Some(Phase::Drogue), 12.0, 16, 1);
    let v = prediction_json(&m, &fix, &p, 12.0, Some((31.75, -102.05)));
    let ov = &v["overlay"];
    assert_eq!(ov["kind"], "los_prediction");
    assert_eq!(ov["label"], LABEL);
    let kinds: Vec<&str> = ov["markers"].as_array().unwrap().iter().map(|m| m["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["last_fix", "predicted_position", "landing"]);
    let areas: Vec<&str> = ov["areas"].as_array().unwrap().iter().map(|a| a["kind"].as_str().unwrap()).collect();
    assert_eq!(areas, ["position_uncertainty", "landing_ellipse"]);
    assert_eq!(ov["tracks"][0]["phase"], "drogue");
    assert!(v["from_station"]["distance_m"].as_f64().unwrap() > 100.0);
    assert!(v["landing"]["in_s"].as_f64().unwrap() > 30.0);
    assert_eq!(v["phase"], "drogue");
}

#[test]
fn the_last_fix_velocity_comes_from_the_track() {
    let o = Origin { lat_deg: SITE.lat_deg, lon_deg: SITE.lon_deg, alt_msl_m: SITE.alt_msl_m };
    let mut fixes = VecDeque::new();
    for k in 0..=30 {
        let t = k as f64 * 0.1;
        let (lat, lon) = o.lat_lon(4.0 * t, -2.0 * t);
        fixes.push_back(Gps { t_ms: (t * 1000.0) as i64, lat, lon, alt: 2000.0 - 25.0 * t, rf: false });
    }
    let f = last_fix(&fixes, &o).unwrap();
    assert!((f.ve_mps.unwrap() - 4.0).abs() < 0.05 && (f.vn_mps.unwrap() + 2.0).abs() < 0.05 && (f.vu_mps.unwrap() + 25.0).abs() < 0.05, "{f:?}");
    assert_eq!(f.t_ms, 3000);
}

/// The live state machine: packets stop in flight, the station predicts; a
/// GPS fix comes back and the error is logged. (The module state is global, so
/// this is the only test that drives it.) It runs in its own test process:
/// other tests reload that global state (`igt::load_persisted` points the log
/// at their own directory, rows from any FC sender reach `observe_row`), and
/// under host load such a reload landed mid-test and moved the log away.
#[tokio::test]
async fn live_loss_and_return_of_signal() {
    if crate::operator_queue::isolated_case(module_path!(), "live_loss_and_return_of_signal") {
        return;
    }
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    load(root.0.clone());
    *state.state.lock().unwrap() = FlightState::Descent;
    let gps = mock_gps();
    let now = now_ms();
    // The track up to 100 s, received 20 s ago.
    let base = now - 20_000 - 100_000;
    for g in gps.iter().filter(|g| g.0 >= 90.0 && g.0 <= 100.0) {
        let row = TelemetryRow::marked(base + (g.0 * 1000.0) as i64, "GPS_DATA".into(), "FC".into(), vec![Some(g.1 as f32), Some(g.2 as f32), Some(g.3 as f32)]);
        observe_row(&row);
    }
    tick(&state);
    let v = view(&state);
    assert_eq!(v["state"], "predicting", "{v}");
    assert_eq!(v["label"], LABEL);
    assert!(v["overlay"]["markers"].is_array());
    assert!(v["since_fix_s"].as_f64().unwrap() >= 19.0);
    assert!(watch_target().is_some());
    // The predicted landing area is now the recovery search's target.
    let search = state.recovery_search.status_body(now);
    assert_eq!(search["target"]["source"], "los_prediction", "{search}");
    // Signal back: a fix at the true position 20 s after the last one.
    let truth = gps_at(&gps, 120.0);
    observe_row(&TelemetryRow::marked(now, "GPS_DATA".into(), "FC".into(), vec![Some(truth.1 as f32), Some(truth.2 as f32), Some(truth.3 as f32)]));
    tick(&state);
    let v = view(&state);
    assert_eq!(v["state"], "resolved", "{v}");
    let c = &v["comparison"];
    assert_eq!(c["resolved_by"], "gps_fix");
    let err = c["error_m"].as_f64().unwrap();
    assert!(err < 300.0, "{c}");
    assert_eq!(v["history"].as_array().unwrap().len(), 1);
    assert!(watch_target().is_none());
    let log = std::fs::read_to_string(root.0.join(DIR).join(LOG_FILE)).unwrap();
    assert_eq!(log.lines().count(), 1);
    // Out of flight: idle.
    *state.state.lock().unwrap() = FlightState::Landed;
    tick(&state);
    assert_ne!(view(&state)["state"], "predicting");
}

#[tokio::test]
async fn simulate_validates_and_predicts() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    let body = json!({"last_fix": {"t_ms": 0, "lat_deg": 31.76, "lon_deg": -102.04, "alt_msl_m": 2500.0}, "phase": "drogue", "elapsed_s": 30.0, "site": SITE});
    let v = simulate(&state, body.to_string().as_bytes()).unwrap();
    assert_eq!(v["state"], "predicting");
    assert_eq!(v["simulated"], true);
    assert!(v["predicted_now"]["alt_agl_m"].as_f64().unwrap() < 1610.0);
    assert!(simulate(&state, br#"{"last_fix": {"t_ms": 0, "lat_deg": 91, "lon_deg": 0, "alt_msl_m": 0}, "elapsed_s": 1}"#).is_err());
    assert!(simulate(&state, br#"{"last_fix": {"t_ms": 0, "lat_deg": 1, "lon_deg": 0, "alt_msl_m": 0}, "elapsed_s": 1, "phase": "orbit"}"#).is_err());
}
