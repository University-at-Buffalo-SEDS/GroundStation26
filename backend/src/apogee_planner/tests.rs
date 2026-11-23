use super::*;
use crate::auth::{AuthPrincipal, CalibrationAccess, CommandAccess, Permissions, StreamRole};
use crate::station_mode::Mode;
use std::io::Write;

fn principal(admin: bool, send: bool) -> AuthPrincipal {
    AuthPrincipal {
        roles: if admin { vec![StreamRole::Admin] } else { Vec::new() },
        username: Some(if admin { "admin" } else { "operator" }.into()),
        permissions: Permissions { view_data: true, send_commands: send, ..Default::default() },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess { allowed_commands: vec![] },
        calibration_access: CalibrationAccess::default(),
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "seds-apogee-{tag}-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ------------------------------------------------------------ the committed model copies

#[test]
fn model_copies_match_tools_flight_profile() {
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/flight_profile");
    assert_eq!(flight::VEHICLE_JSON, std::fs::read_to_string(tools.join("vehicle.json")).unwrap(), "copy tools/flight_profile/vehicle.json to backend/src/apogee_planner/model/");
    assert_eq!(flight::THRUST_CSV, std::fs::read_to_string(tools.join("motors/testfire_20261116.csv")).unwrap(), "copy the thrust curve to backend/src/apogee_planner/model/");
    let reference = flight::Motor::from_config(&flight::Config::reference().motor, flight::THRUST_CSV);
    assert!((reference.impulse - motor::REF_IMPULSE_NS).abs() < 1.0, "REF_IMPULSE_NS follows the curve: {}", reference.impulse);
}

// ------------------------------------------------------------ flight model vs tools/flight_profile

fn set_path(cfg: &mut Value, dotted: &str, value: &Value) {
    let mut node = cfg;
    let keys: Vec<&str> = dotted.split('.').collect();
    for k in &keys[..keys.len() - 1] {
        node = node.get_mut(*k).unwrap();
    }
    assert!(node.get(keys[keys.len() - 1]).is_some(), "{dotted}");
    node[keys[keys.len() - 1]] = value.clone();
}

#[test]
fn planner_flight_matches_the_python_reference_cases() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/reference_cases.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let mut cfg: Value = serde_json::from_str(flight::VEHICLE_JSON).unwrap();
        for (k, v) in case["overrides"].as_object().unwrap() {
            set_path(&mut cfg, k, v);
        }
        let cfg: flight::Config = serde_json::from_value(cfg).unwrap();
        let motor = flight::Motor::from_config(&cfg.motor, flight::THRUST_CSV);
        let env = flight::Environment::from_config(&cfg);
        let r = flight::fly(&cfg, &env, &motor, 0.001);
        let e = &case["expect"];
        let close = |got: f64, want: f64, rel: f64, what: &str| {
            assert!((got - want).abs() <= rel * want.abs().max(1e-6), "{name} {what}: planner {got:.3} vs flightsim.py {want:.3}");
        };
        close(motor.impulse, e["impulse_ns"].as_f64().unwrap(), 1e-4, "impulse");
        close(r.apogee_m, e["apogee_m"].as_f64().unwrap(), 0.005, "apogee");
        close(r.apogee_s, e["apogee_s"].as_f64().unwrap(), 0.01, "apogee time");
        close(r.rail_exit_mps, e["rail_exit_mps"].as_f64().unwrap(), 0.01, "rail exit");
        close(r.max_mach, e["max_mach"].as_f64().unwrap(), 0.01, "max Mach");
    }
}

#[test]
fn descent_drifts_downwind() {
    let cfg = flight::Config::reference();
    let motor = flight::Motor::from_config(&cfg.motor, flight::THRUST_CSV);
    let env = flight::Environment::from_config(&cfg);
    let r = flight::fly(&cfg, &env, &motor, 0.002);
    // Wind from 300° blows toward 120°: the landing is to the east-south-east.
    assert!((60.0..180.0).contains(&r.drift_bearing_deg), "{}", r.drift_bearing_deg);
    assert!(r.drift_m > 200.0 && r.drift_m < 3000.0, "{}", r.drift_m);
}

// ------------------------------------------------------------ weather

const FIXTURE_SITE: weather::Site = weather::Site { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.016 };
const NOON_UTC_MS: i64 = 1_795_435_200_000; // 2026-11-23T12:00Z

#[test]
fn open_meteo_fixture_parses_to_the_nearest_hour_with_winds_aloft() {
    let body = include_str!("fixtures/open_meteo_20261122.json");
    let url = weather::open_meteo_url(FIXTURE_SITE);
    let s = weather::parse_open_meteo(body, FIXTURE_SITE, NOON_UTC_MS + 20 * 60_000, 1, &url).unwrap();
    assert_eq!(s.valid_time_ms, NOON_UTC_MS, "nearest hour");
    assert_eq!(s.provider, "open_meteo");
    assert!((s.surface_temp_c - 1.9).abs() < 1e-9);
    assert!((s.surface_pressure_pa.unwrap() - 92_470.0).abs() < 1e-6);
    // Levels below the site are dropped, the rest are AGL and ascending.
    assert!(!s.levels.is_empty());
    assert!(s.levels.iter().all(|l| l.h_agl_m >= 50.0));
    assert!(s.levels.windows(2).all(|w| w[0].h_agl_m < w[1].h_agl_m));
    let l900 = s.levels.iter().find(|l| (l.h_agl_m - (1094.0 - 890.016)).abs() < 1e-6).expect("900 hPa level");
    assert!((l900.speed_mps - 10.74).abs() < 1e-9 && (l900.from_deg - 68.0).abs() < 1e-9);
    assert_eq!(s.request_url.as_deref(), Some(url.as_str()));
    // The environment reproduces the observed surface.
    let env = s.environment(0.14, 1.4);
    let (t, p, ..) = flight::atmosphere(FIXTURE_SITE.alt_msl_m, env.temp_offset_k, env.p_sl_pa);
    assert!((t - 275.05).abs() < 0.01 && (p - 92_470.0).abs() < 1.0);
}

#[test]
fn open_meteo_errors_and_garbage_are_refused() {
    assert!(weather::parse_open_meteo(r#"{"error":true,"reason":"bad"}"#, FIXTURE_SITE, 0, 0, "").unwrap_err().contains("bad"));
    assert!(weather::parse_open_meteo("not json", FIXTURE_SITE, 0, 0, "").is_err());
    assert!(weather::parse_open_meteo(r#"{"hourly":{"time":[]}}"#, FIXTURE_SITE, 0, 0, "").is_err());
}

#[test]
fn the_online_request_carries_only_the_rounded_site_and_variables() {
    let url = weather::open_meteo_url(weather::Site { lat_deg: 43.001234, lon_deg: -78.786543, alt_msl_m: 180.0 });
    assert!(url.starts_with("https://api.open-meteo.com/v1/forecast?latitude=43.00&longitude=-78.79&hourly="), "{url}");
    assert!(!url.contains("43.0012") && !url.contains("key") && !url.contains("180"));
}

#[test]
fn manual_weather_validates_and_builds_a_profile() {
    let mut m = weather::ManualWeather { wind_1km_mps: Some(10.0), wind_1km_from_deg: Some(270.0), ..Default::default() };
    assert!(m.validate().is_ok());
    let s = m.snapshot(FIXTURE_SITE, 5, 6);
    assert_eq!(s.levels.len(), 1);
    assert!(s.surface_pressure_pa.unwrap() > 85_000.0, "standard pressure filled in");
    let env = s.environment(0.14, 1.4);
    let (e, _) = env.wind.at(1000.0);
    assert!((e - 10.0).abs() < 1e-6, "1 km wind from 270 blows east");
    m.wind_mps = 99.0;
    assert!(m.validate().is_err());
}

// ------------------------------------------------------------ OpenRocket import

const ORK_XML: &str = r#"<?xml version='1.0' encoding='utf-8'?>
<openrocket version="1.9" creator="synthetic test">
  <rocket>
    <name>Synthetic 6in</name>
    <subcomponents>
      <stage>
        <name>Sustainer</name>
        <subcomponents>
          <nosecone>
            <name>Nose &amp; tip</name>
            <finish>smooth</finish>
            <material type="bulk" density="1850.0">Fiberglass</material>
            <length>0.762</length>
            <thickness>0.003</thickness>
            <shape>ogive</shape>
            <aftradius>auto 0.0762</aftradius>
          </nosecone>
          <bodytube>
            <name>Body</name>
            <material type="bulk" density="1850.0">Fiberglass</material>
            <length>3.81</length>
            <thickness>0.003</thickness>
            <radius>0.0762</radius>
            <subcomponents>
              <masscomponent><name>Run tank</name><position type="top">1.0</position><length>0.8</length><mass>7.0</mass></masscomponent>
              <masscomponent><name>Avionics</name><axialoffset method="top">0.2</axialoffset><position type="top">0.2</position><length>0.3</length><mass>2.5</mass></masscomponent>
              <innertube><name>Chamber</name><position type="bottom">0.0</position><length>1.0</length><outerradius>0.05</outerradius><thickness>0.004</thickness>
                <material type="bulk" density="2700.0">Aluminum</material><motormount><ignitionevent>automatic</ignitionevent></motormount></innertube>
              <trapezoidfinset><name>Fins</name><position type="bottom">0.0</position><material type="bulk" density="1850.0">Fiberglass</material>
                <fincount>4</fincount><rootchord>0.3</rootchord><tipchord>0.1</tipchord><height>0.15</height><sweeplength>0.15</sweeplength><thickness>0.0048</thickness></trapezoidfinset>
              <parachute><name>Main</name><position type="top">0.5</position><length>0.2</length><diameter>2.4</diameter><material type="surface" density="0.067">Ripstop nylon</material></parachute>
            </subcomponents>
          </bodytube>
        </subcomponents>
      </stage>
    </subcomponents>
  </rocket>
  <simulations/>
</openrocket>
"#;

fn zip_one(name: &str, data: &[u8], deflate: bool) -> Vec<u8> {
    let payload = if deflate {
        let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    } else {
        data.to_vec()
    };
    let crc = crc32fast::hash(data);
    let method: u16 = if deflate { 8 } else { 0 };
    let mut out = Vec::new();
    let local = |out: &mut Vec<u8>| {
        out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0]);
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
    };
    local(&mut out);
    out.extend_from_slice(&payload);
    let cd = out.len();
    out.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0]);
    out.extend_from_slice(&method.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    let cd_len = out.len() - cd;
    out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 1, 0, 1, 0]);
    out.extend_from_slice(&(cd_len as u32).to_le_bytes());
    out.extend_from_slice(&(cd as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

#[test]
fn ork_import_reads_zip_gzip_and_plain_files() {
    let reference = flight::Config::reference().vehicle;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(ORK_XML.as_bytes()).unwrap();
    let forms = [
        zip_one("rocket.ork", ORK_XML.as_bytes(), true),
        zip_one("rocket.ork", ORK_XML.as_bytes(), false),
        gz.finish().unwrap(),
        ORK_XML.as_bytes().to_vec(),
    ];
    let mut first = None;
    for bytes in forms {
        let i = ork::import(&bytes, &reference).unwrap();
        assert_eq!(first.get_or_insert_with(|| i.clone()), &i, "all forms import the same");
    }
    let i = first.unwrap();
    assert_eq!(i.name, "Synthetic 6in");
    let v = &i.vehicle;
    assert!((v.length_m - 4.572).abs() < 1e-9);
    assert!((v.diameter_m - 0.1524).abs() < 1e-9);
    assert!((v.nose_length_m - 0.762).abs() < 1e-9);
    assert_eq!(v.fins, flight::Fins { count: 4.0, root_m: 0.3, tip_m: 0.1, span_m: 0.15, thickness_m: 0.0048 });
    assert!((v.surface_roughness_m - 20e-6).abs() < 1e-12, "smooth finish");
    // Body tube 3.81 m of 3 mm fiberglass: pi*(r^2-(r-t)^2)*L*rho.
    let tube = std::f64::consts::PI * (0.0762_f64.powi(2) - 0.0732_f64.powi(2)) * 3.81 * 1850.0;
    let body = i.components.iter().find(|c| c.name == "Body").unwrap();
    assert!((body.mass_kg - tube).abs() < 1e-9);
    assert!(i.components.iter().any(|c| c.name == "Nose & tip"), "entities decoded");
    let tank = i.components.iter().find(|c| c.name == "Run tank").unwrap();
    assert!((tank.cg_from_nose_m - (0.762 + 1.0 + 0.4)).abs() < 1e-9, "position relative to the parent top");
    let chute = i.components.iter().find(|c| c.kind == "parachute").unwrap();
    assert!((chute.mass_kg - 0.067 * std::f64::consts::PI * 1.44).abs() < 1e-9);
    let total: f64 = i.components.iter().map(|c| c.mass_kg).sum();
    assert!((v.dry_mass_kg - total).abs() < 1e-9);
    assert!((15.0..30.0).contains(&v.dry_mass_kg), "{}", v.dry_mass_kg);
    assert_eq!(i.motor_mount, Some(ork::MotorMount { length_m: 1.0, diameter_m: 0.092 }));
    assert!(i.cp_from_nose_m > i.cg_from_nose_m, "stable");
    assert!(v.cn_alpha_per_rad > 6.0 && v.cn_alpha_per_rad < 20.0, "{}", v.cn_alpha_per_rad);
    assert!(v.static_margin_cal > 1.0, "{}", v.static_margin_cal);
}

#[test]
fn ork_import_honours_overrides_and_refuses_junk() {
    let reference = flight::Config::reference().vehicle;
    let with_override = ORK_XML.replace("<name>Sustainer</name>", "<name>Sustainer</name><overridemass>33.0</overridemass>");
    let i = ork::import(with_override.as_bytes(), &reference).unwrap();
    assert!((i.vehicle.dry_mass_kg - 33.0).abs() < 1e-9);
    assert!(i.warnings.iter().any(|w| w.contains("override mass")));
    assert!(ork::import(b"PK\x03\x04garbage", &reference).is_err());
    assert!(ork::import(b"<notrocket/>", &reference).unwrap_err().contains("OpenRocket"));
    assert!(ork::import(b"<openrocket><rocket><subcomponents></subcomponents></rocket></openrocket>", &reference).unwrap_err().contains("stages"));
    assert!(ork::import(b"<openrocket><rocket>", &reference).is_err());
}

// ------------------------------------------------------------ solver

fn solver_input(target_m: f64) -> solver::Input {
    let w = weather::ManualWeather { surface_temp_c: 15.0, ..Default::default() }.snapshot(FIXTURE_SITE, 0, 0);
    solver::Input {
        cfg: flight::Config::reference(),
        weather: w,
        fit: motor::MotorFit::fit(&[motor::FitPoint::reference()]),
        target_m,
        tank_c: 15.0,
        tank_volume_l: 12.0,
        min_ullage: 0.15,
        min_fill_kg: 3.0,
        max_supercharge_psi: 800.0,
        limit_sources: vec!["test".into()],
        nominal_supercharge_psi: 750.0,
        launch_ms: 0,
        dt: 0.002,
    }
}

#[test]
fn solver_converges_on_a_reachable_target_within_limits() {
    let input = solver_input(2000.0);
    let s = solver::solve(&input);
    assert_eq!(s.status, "ok", "{:?}", s.warnings);
    assert!((s.apogee_m - 2000.0).abs() < 40.0, "{}", s.apogee_m);
    assert!(s.fill_mass_kg >= 3.0 && s.fill_mass_kg <= s.limits.max_fill_kg);
    assert!((s.supercharge_psi - 750.0).abs() < 1e-9, "fill first: pressure stays nominal");
    assert!(s.apogee_band_m[0] < s.apogee_m && s.apogee_m < s.apogee_band_m[1]);
    assert!(s.apogee_sigma_m > 20.0, "one fire: a real band, not a point");
    assert!(s.fill_band_kg[0] < s.fill_mass_kg && s.fill_mass_kg < s.fill_band_kg[1]);
    assert!(s.time_to_apogee_s > 10.0 && s.max_mach > 0.2 && s.drift_m > 0.0);
    // Rounded to the fill step.
    assert!(((s.fill_mass_kg / solver::FILL_STEP_KG).round() * solver::FILL_STEP_KG - s.fill_mass_kg).abs() < 1e-9);
}

#[test]
fn unreachable_targets_report_the_reach_and_never_exceed_the_limits() {
    let high = solver::solve(&solver_input(6000.0));
    assert_eq!(high.status, "unreachable_high");
    assert!((high.fill_mass_kg - high.limits.max_fill_kg).abs() < 1e-9);
    assert!(high.supercharge_psi <= 800.0);
    assert!(high.max_achievable_apogee_m < 6000.0 && high.max_achievable_apogee_m > 1000.0);
    assert!(high.warnings.iter().any(|w| w.contains("not reachable")));
    let low = solver::solve(&solver_input(100.0));
    assert_eq!(low.status, "unreachable_low");
    assert!((low.fill_mass_kg - 3.0).abs() < 1e-9);
    assert!(low.min_achievable_apogee_m > 100.0);
}

#[test]
fn the_pressure_limit_caps_the_supercharge_and_the_ullage_rule_caps_the_fill() {
    // Needs more than max fill at nominal: pressure rises, but only to the limit.
    let mut input = solver_input(0.0);
    input.max_supercharge_psi = 760.0;
    input.nominal_supercharge_psi = 740.0;
    let reach = solver::solve(&solver_input(6000.0)).max_achievable_apogee_m;
    input.target_m = reach; // beyond what 760 psi reaches
    let s = solver::solve(&input);
    assert!(s.supercharge_psi <= 760.0 + 1e-9);
    assert!(s.limits.max_supercharge_psi <= 760.0);
    // A warmer tank holds less liquid.
    let mut warm = solver_input(2000.0);
    warm.tank_c = 30.0;
    assert!(solver::limits(&warm).max_fill_kg < solver::limits(&solver_input(2000.0)).max_fill_kg);
    // MEOP below the nominal supercharge is clamped with a warning.
    let mut meop = solver_input(2000.0);
    meop.max_supercharge_psi = 700.0;
    let s = solver::solve(&meop);
    assert!(s.supercharge_psi <= 700.0 + 1e-9);
    assert!(s.warnings.iter().any(|w| w.contains("limit")));
}

#[test]
fn the_planner_uses_the_fitted_model() {
    let base = solver::solve(&solver_input(2000.0));
    // A second fire showing a stronger motor lowers the fill for the same apogee.
    let mut input = solver_input(2000.0);
    let strong = motor::FitPoint { id: "f2".into(), label: "f2".into(), fill_kg: 6.0, mean_pressure_pa: motor::reference_mean_pressure_pa(), impulse_ns: 12_000.0, impulse_sigma_ns: 400.0 };
    input.fit = motor::MotorFit::fit(&[motor::FitPoint::reference(), strong]);
    let s = solver::solve(&input);
    assert!(s.fill_mass_kg < base.fill_mass_kg - 0.2, "{} vs {}", s.fill_mass_kg, base.fill_mass_kg);
    assert!(s.impulse_sigma_ns / s.impulse_ns < base.impulse_sigma_ns / base.impulse_ns, "two fires: a narrower band");
}

// ------------------------------------------------------------ approval flow

struct Rig {
    state: Arc<AppState>,
    fill_path: PathBuf,
    _mode: crate::runtime_mode::test_support::ThreadMode,
}

async fn rig(mode: Mode) -> Rig {
    let guard = crate::runtime_mode::test_support::thread_mode(mode);
    let state = crate::state::tests::test_app_state().await;
    let dir = temp_dir("apply");
    let fill_path = dir.join("fill_targets.json");
    crate::fill_targets::TEST_CONFIG_PATH.with(|p| *p.borrow_mut() = Some(fill_path.clone()));
    load(&state, dir);
    state.set_local_flight_state(FlightState::Idle);
    let mut settings = Settings { target_apogee_m: 2000.0, ..Default::default() };
    settings.manual_weather.surface_temp_c = 15.0;
    update_settings(&state, &principal(true, true), settings).unwrap();
    Rig { state, fill_path, _mode: guard }
}

#[tokio::test]
async fn apply_writes_the_fill_targets_through_the_manual_path_and_logs_it() {
    let r = rig(Mode::Normal).await;
    let before = r.state.fill_targets.lock().unwrap().clone();
    let plan = compute(&r.state, Some(&principal(false, true))).unwrap();
    assert_eq!(plan.solution.status, "ok");
    let applied = apply(&r.state, &principal(false, true), &plan.id).unwrap();
    let saved: crate::fill_targets::FillTargetsConfig = serde_json::from_slice(&std::fs::read(&r.fill_path).unwrap()).unwrap();
    assert!((f64::from(saved.nitrous.target_mass_kg) - plan.solution.fill_mass_kg).abs() < 1e-4);
    assert!((f64::from(saved.nitrous.target_pressure_psi) - plan.solution.supercharge_psi).abs() < 1e-3);
    assert_eq!(saved.nitrogen, before.nitrogen, "only the nitrous targets change");
    assert_eq!(saved.fill_source, before.fill_source);
    assert_eq!(*r.state.fill_targets.lock().unwrap(), saved, "in-memory targets follow");
    assert_eq!(applied.applied_by.as_deref(), Some("operator"));
    assert!((applied.previous_targets.nitrous_target_mass_kg - f64::from(before.nitrous.target_mass_kg)).abs() < 1e-6);
    let dir = rt(&r.state).inner.lock().unwrap().data_dir.clone().unwrap();
    let log = std::fs::read_to_string(dir.join(DIR).join(APPLIED_LOG)).unwrap();
    assert!(log.contains(&plan.id) && log.contains("\"by\":\"operator\""));
    // Recorded for the launch capture.
    let details = capture_details(&r.state);
    assert_eq!(details["plan_id"], plan.id);
    assert!(details["predicted_apogee_m"].as_f64().unwrap() > 1000.0);
    // The same plan cannot be applied after its inputs change.
    let mut s = rt(&r.state).inner.lock().unwrap().settings.clone();
    s.target_apogee_m = 1800.0;
    update_settings(&r.state, &principal(true, true), s).unwrap();
    let err = apply(&r.state, &principal(false, true), &plan.id).unwrap_err();
    assert_eq!(err.0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn apply_is_refused_mid_fill_and_later() {
    let r = rig(Mode::Normal).await;
    let plan = compute(&r.state, None).unwrap();
    for fs in [FlightState::NitrousFill, FlightState::NitrogenFill, FlightState::FillTest, FlightState::Armed, FlightState::Launch] {
        r.state.set_local_flight_state(fs);
        let err = apply(&r.state, &principal(false, true), &plan.id).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT, "{fs:?}");
        assert!(err.1.contains("Idle or PreFill"), "{}", err.1);
        assert!(!r.fill_path.exists(), "nothing written");
    }
    r.state.set_local_flight_state(FlightState::PreFill);
    r.state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Filling;
    let err = apply(&r.state, &principal(false, true), &plan.id).unwrap_err();
    assert_eq!(err.0, StatusCode::CONFLICT);
    r.state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Idle;
    assert!(apply(&r.state, &principal(false, true), &plan.id).is_ok(), "PreFill and idle GSE: allowed");
}

#[tokio::test]
async fn apply_is_refused_in_test_fire_and_testing_modes_but_compute_works() {
    for mode in [Mode::TestFire, Mode::Testing] {
        let r = rig(mode).await;
        let plan = compute(&r.state, None).expect("compute is available in every mode");
        let err = apply(&r.state, &principal(true, true), &plan.id).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT, "{mode:?}");
        assert!(err.1.contains("only in Normal, Integrated ground test and HITL"), "{}", err.1);
        let v = view(&r.state);
        assert_eq!(v["apply_gate"]["allowed"], false);
        assert!(v["apply_gate"]["reason"].as_str().unwrap().contains("never changes the fill targets"));
        assert!(!r.fill_path.exists());
    }
    for mode in [Mode::Normal, Mode::IntegratedGroundTest, Mode::Hitl] {
        let r = rig(mode).await;
        assert_eq!(view(&r.state)["apply_gate"]["allowed"], true, "{mode:?}");
        let plan = compute(&r.state, None).unwrap();
        assert!(apply(&r.state, &principal(false, true), &plan.id).is_ok(), "{mode:?}");
    }
}

#[tokio::test]
async fn permissions_unreachable_and_stale_plans_are_refused() {
    let r = rig(Mode::Normal).await;
    let plan = compute(&r.state, None).unwrap();
    assert_eq!(apply(&r.state, &principal(false, false), &plan.id).unwrap_err().0, StatusCode::FORBIDDEN);
    assert_eq!(apply(&r.state, &principal(false, true), "plan-other").unwrap_err().0, StatusCode::CONFLICT);
    assert_eq!(update_settings(&r.state, &principal(false, true), Settings::default()).unwrap_err().0, StatusCode::FORBIDDEN);
    let bad = Settings { target_apogee_m: -5.0, ..Default::default() };
    assert_eq!(update_settings(&r.state, &principal(true, true), bad).unwrap_err().0, StatusCode::BAD_REQUEST);
    // Stale.
    rt(&r.state).inner.lock().unwrap().last_plan.as_mut().unwrap().computed_ms -= PLAN_MAX_AGE_MS + 1;
    assert!(apply(&r.state, &principal(false, true), &plan.id).unwrap_err().1.contains("30 minutes"));
    // Unreachable.
    let mut s = rt(&r.state).inner.lock().unwrap().settings.clone();
    s.target_apogee_m = 9000.0;
    update_settings(&r.state, &principal(true, true), s).unwrap();
    let plan = compute(&r.state, None).unwrap();
    assert_eq!(plan.solution.status, "unreachable_high");
    assert!(apply(&r.state, &principal(false, true), &plan.id).unwrap_err().1.contains("not reachable"));
    assert!(!r.fill_path.exists());
}

#[tokio::test]
async fn open_meteo_source_needs_a_cached_forecast_and_the_opt_in() {
    let r = rig(Mode::Normal).await;
    let mut s = rt(&r.state).inner.lock().unwrap().settings.clone();
    s.weather_source = "open_meteo".into();
    assert!(update_settings(&r.state, &principal(true, true), s.clone()).is_err(), "not without the opt-in");
    s.online_weather_enabled = true;
    update_settings(&r.state, &principal(true, true), s).unwrap();
    assert_eq!(compute(&r.state, None).unwrap_err().0, StatusCode::CONFLICT, "no forecast cached yet");
    let snap = weather::parse_open_meteo(include_str!("fixtures/open_meteo_20261122.json"), FIXTURE_SITE, NOON_UTC_MS, NOON_UTC_MS, "u").unwrap();
    store_weather(&r.state, &snap);
    let plan = compute(&r.state, None).unwrap();
    assert_eq!(plan.weather.provider, "open_meteo");
    assert!((plan.tank_temp_c - 1.9).abs() < 1e-9, "ambient drives the N2O temperature");
    assert!(plan.solution.limits.vapour_pressure_psi < 500.0, "cold day: low vapour pressure");
    let sim = sim_environment(&r.state);
    assert!((sim["environment"]["ambient_temp_c"].as_f64().unwrap() - 1.9).abs() < 1e-9);
    assert!(!sim["environment"]["winds_aloft"].as_array().unwrap().is_empty());
    let conditions = &sim["firmware_sim_conditions"];
    assert!((conditions["ambient_c"].as_f64().unwrap() - 1.9).abs() < 1e-9);
    // The firmware sim accepts them as its plant conditions.
    assert!(crate::firmware_sim_mode::validate_control(&json!({"conditions": conditions})).is_ok());
    // Not running simulated firmware: refused, nothing written.
    assert_eq!(rehearse_in_firmware_sim(&r.state, &principal(true, true)).unwrap_err().0, StatusCode::CONFLICT);
    assert_eq!(rehearse_in_firmware_sim(&r.state, &principal(false, true)).unwrap_err().0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn settings_vehicle_and_calibration_persist() {
    let r = rig(Mode::Normal).await;
    let dir = rt(&r.state).inner.lock().unwrap().data_dir.clone().unwrap();
    import_ork(&r.state, &principal(true, true), ORK_XML.as_bytes(), Some("synthetic.ork".into())).unwrap();
    assert!(import_ork(&r.state, &principal(false, true), ORK_XML.as_bytes(), None).is_err(), "admin only");
    let plan = compute(&r.state, None).unwrap();
    assert!(plan.vehicle_source.contains("Synthetic 6in"));
    let applied = apply(&r.state, &principal(false, true), &plan.id).unwrap();
    let v = calibrate_flight(&r.state, &principal(false, true), Some(applied.plan.solution.apogee_m * 0.9)).unwrap();
    let k = v["flight_calibration"]["cd_scale"].as_f64().unwrap();
    assert!(k > 1.05, "lower actual apogee means more drag: {k}");
    // Reload from disk.
    let again = crate::state::tests::test_app_state().await;
    load(&again, dir);
    let inner = rt(&again).inner.lock().unwrap();
    assert!((inner.settings.target_apogee_m - 2000.0).abs() < 1e-9);
    assert_eq!(inner.vehicle.as_ref().unwrap().file_name.as_deref(), Some("synthetic.ork"));
    assert_eq!(inner.calibration.points.len(), 1);
    assert_eq!(inner.applied.as_ref().unwrap().plan.id, plan.id);
    assert!(!inner.fit_history.is_empty());
}

/// `cargo test -p groundstation_backend print_reference_plans -- --ignored --nocapture`
#[test]
#[ignore]
fn print_reference_plans() {
    for (t, target) in [(20.0, 3048.0), (5.0, 3048.0), (20.0, 2500.0), (30.0, 2500.0)] {
        let mut input = solver_input(target);
        input.tank_c = t;
        input.weather.surface_temp_c = t;
        input.dt = 0.001;
        let s = solver::solve(&input);
        println!(
            "{t:>4} C target {target:.0} m: {} fill {:.2} kg sc {:.0} psi apogee {:.0} ({:.0}..{:.0}) reach {:.0} rail {:.1} mach {:.2} drift {:.0} I {:.0}±{:.0} pv {:.0} maxfill {:.2} flights {}\n   {:?}",
            s.status, s.fill_mass_kg, s.supercharge_psi, s.apogee_m, s.apogee_band_m[0], s.apogee_band_m[1], s.max_achievable_apogee_m,
            s.rail_exit_mps, s.max_mach, s.drift_m, s.impulse_ns, s.impulse_sigma_ns, s.limits.vapour_pressure_psi, s.limits.max_fill_kg, s.flights, s.warnings
        );
    }
}

#[test]
fn archive_fires_become_fit_points_and_the_seed_reproduces_the_reference() {
    let seed = crate::fire_archive::FitInput {
        id: "2026-11-16_static_fire".into(),
        label: "seed".into(),
        fill_kg: 5.65,
        tank_psi_at_ignition: Some(404.0),
        supercharge_psi: None,
        ambient_c: None,
        impulse_ns: 9192.0,
        impulse_sigma_ns: 900.0,
    };
    let pts = fit_points_from(vec![seed.clone()]);
    assert!((pts[0].mean_pressure_pa / motor::reference_mean_pressure_pa() - 1.0).abs() < 1e-6);
    let fit = motor::MotorFit::fit(&pts);
    assert!((fit.c_ref_ns_per_kg - 9192.0 / 5.65).abs() < 0.01);
    // A supercharged fire sits at a higher mean pressure.
    let hot = crate::fire_archive::FitInput { id: "b".into(), supercharge_psi: Some(750.0), tank_psi_at_ignition: Some(750.0), ..seed };
    let pts = fit_points_from(vec![hot]);
    assert!(pts[0].mean_pressure_pa > motor::reference_mean_pressure_pa() * 1.3);
    assert_eq!(fit_points_from(Vec::new()), vec![motor::FitPoint::reference()], "empty archive: the reference fire");
}
