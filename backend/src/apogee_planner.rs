//! Apogee planner (docs/apogee-planner.md): target apogee + weather + motor
//! fit + vehicle -> N2O fill mass and supercharge pressure targets, applied to
//! the fill targets only on an explicit, logged operator approval.
//!
//! Nothing here actuates anything.
use crate::auth::{AuthPrincipal, Permission};
use crate::state::AppState;
use crate::types::FlightState;
use axum::{
    Json,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[path = "apogee_planner/flight.rs"]
pub(crate) mod flight;
#[path = "apogee_planner/motor.rs"]
pub(crate) mod motor;
#[path = "apogee_planner/n2o.rs"]
pub(crate) mod n2o;
#[path = "apogee_planner/ork.rs"]
pub(crate) mod ork;
#[path = "apogee_planner/solver.rs"]
pub(crate) mod solver;
#[path = "apogee_planner/weather.rs"]
pub(crate) mod weather;
#[cfg(test)]
#[path = "apogee_planner/tests.rs"]
mod tests;

const DIR: &str = "apogee_planner";
const SETTINGS_FILE: &str = "settings.json";
const WEATHER_FILE: &str = "weather_cache.json";
const VEHICLE_FILE: &str = "vehicle_import.json";
const CALIBRATION_FILE: &str = "flight_calibration.json";
const APPLIED_FILE: &str = "applied.json";
const APPLIED_LOG: &str = "applied.jsonl";
const FIT_HISTORY_FILE: &str = "fit_history.json";
/// A plan older than this must be recomputed before it can be applied.
pub(crate) const PLAN_MAX_AGE_MS: i64 = 30 * 60 * 1000;
const MAX_ORK_BYTES: usize = 32 * 1024 * 1024;
const FT: f64 = 0.3048;

// ------------------------------------------------------------ settings

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub target_apogee_m: f64,
    pub display_unit: String,
    pub weather_source: String,
    pub online_weather_enabled: bool,
    pub launch_time_ms: Option<i64>,
    pub manual_weather: weather::ManualWeather,
    pub tank_temp_c: Option<f64>,
    pub tank_volume_l: f64,
    pub min_ullage_frac: f64,
    pub min_fill_kg: f64,
    pub meop_psi: f64,
    pub nominal_supercharge_psi: Option<f64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_apogee_m: 10_000.0 * FT,
            display_unit: "ft".into(),
            weather_source: "manual".into(),
            online_weather_enabled: false,
            launch_time_ms: None,
            manual_weather: Default::default(),
            tank_temp_c: None,
            tank_volume_l: 12.0,
            min_ullage_frac: 0.15,
            min_fill_kg: 3.0,
            meop_psi: 750.0,
            nominal_supercharge_psi: None,
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<(), String> {
        if !(30.0..=30_000.0).contains(&self.target_apogee_m) {
            return Err("Target apogee must be within 30..30000 m AGL.".into());
        }
        if !matches!(self.display_unit.as_str(), "ft" | "m") {
            return Err("Display unit must be ft or m.".into());
        }
        if !matches!(self.weather_source.as_str(), "manual" | "open_meteo" | "cached") {
            return Err("Weather source must be manual, open_meteo or cached.".into());
        }
        self.manual_weather.validate()?;
        if let Some(t) = self.tank_temp_c {
            if !(-40.0..=35.0).contains(&t) {
                return Err("Tank temperature must be within -40..35 °C.".into());
            }
        }
        if !(1.0..=200.0).contains(&self.tank_volume_l) {
            return Err("Tank volume must be within 1..200 L.".into());
        }
        if !(0.05..=0.5).contains(&self.min_ullage_frac) {
            return Err("Minimum ullage must be within 5..50 %.".into());
        }
        if !(0.5..=100.0).contains(&self.min_fill_kg) {
            return Err("Minimum fill must be within 0.5..100 kg.".into());
        }
        if !(100.0..=2000.0).contains(&self.meop_psi) {
            return Err("MEOP must be within 100..2000 psi.".into());
        }
        if let Some(p) = self.nominal_supercharge_psi {
            if !(0.0..=2000.0).contains(&p) {
                return Err("Nominal supercharge must be within 0..2000 psi.".into());
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ plans

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Targets {
    pub nitrous_target_mass_kg: f64,
    pub nitrous_target_pressure_psi: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FitSummary {
    pub c_ref_ns_per_kg: f64,
    pub n: f64,
    pub sigma_c_rel: f64,
    pub fires: usize,
    #[serde(default)]
    pub fire_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub id: String,
    pub computed_ms: i64,
    pub computed_by: Option<String>,
    #[serde(flatten)]
    pub solution: solver::Solution,
    pub weather: weather::Snapshot,
    pub weather_age_s: f64,
    pub vehicle_source: String,
    pub motor_fit: FitSummary,
    pub current_targets: Targets,
    pub cd_scale: f64,
    pub tank_temp_c: f64,
    pub station_mode: String,
    /// Hash of the inputs; an apply needs the inputs unchanged.
    pub inputs_hash: String,
    pub settings: Settings,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Applied {
    pub plan: Plan,
    pub applied_ms: i64,
    pub applied_by: Option<String>,
    pub previous_targets: Targets,
    pub new_targets: Targets,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CalibrationPoint {
    pub at_ms: i64,
    pub cd_scale: f64,
    pub predicted_m: f64,
    pub actual_m: f64,
    #[serde(default)]
    pub plan_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FlightCalibration {
    pub points: Vec<CalibrationPoint>,
}

impl FlightCalibration {
    pub fn cd_scale(&self) -> f64 {
        if self.points.is_empty() {
            1.0
        } else {
            self.points.iter().map(|p| p.cd_scale).sum::<f64>() / self.points.len() as f64
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FitHistory {
    pub at_ms: i64,
    pub c_ref_ns_per_kg: f64,
    pub n: f64,
    pub sigma_c_rel: f64,
    pub fires: usize,
    pub fire_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VehicleImport {
    pub imported_ms: i64,
    pub file_name: Option<String>,
    pub imported: ork::Imported,
}

// ------------------------------------------------------------ runtime

#[derive(Default)]
struct Inner {
    data_dir: Option<PathBuf>,
    settings: Settings,
    weather_cache: Option<weather::Snapshot>,
    vehicle: Option<VehicleImport>,
    calibration: FlightCalibration,
    last_plan: Option<Plan>,
    applied: Option<Applied>,
    fit_history: Vec<FitHistory>,
}

#[derive(Default)]
pub(crate) struct Runtime {
    inner: Mutex<Inner>,
}

fn rt(state: &AppState) -> &Runtime {
    &state.igt.apogee_planner
}

fn read_json<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Option<T> {
    serde_json::from_slice(&std::fs::read(dir.join(DIR).join(name)).ok()?).ok()
}

fn write_json<T: Serialize>(dir: &Path, name: &str, value: &T) -> std::io::Result<()> {
    use std::io::Write;
    let folder = dir.join(DIR);
    std::fs::create_dir_all(&folder)?;
    let path = folder.join(name);
    let tmp = path.with_extension("pending");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value).unwrap_or_default())?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

fn append_log(dir: &Path, value: &Value) -> std::io::Result<()> {
    use std::io::Write;
    let folder = dir.join(DIR);
    std::fs::create_dir_all(&folder)?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(folder.join(APPLIED_LOG))?;
    writeln!(file, "{}", serde_json::to_string(value).unwrap_or_default())?;
    file.sync_all()
}

pub(crate) fn load(state: &AppState, dir: PathBuf) {
    let mut inner = rt(state).inner.lock().unwrap();
    inner.settings = read_json::<Settings>(&dir, SETTINGS_FILE).filter(|s| s.validate().is_ok()).unwrap_or_default();
    inner.weather_cache = read_json(&dir, WEATHER_FILE);
    inner.vehicle = read_json(&dir, VEHICLE_FILE);
    inner.calibration = read_json(&dir, CALIBRATION_FILE).unwrap_or_default();
    inner.applied = read_json(&dir, APPLIED_FILE);
    inner.fit_history = read_json(&dir, FIT_HISTORY_FILE).unwrap_or_default();
    inner.data_dir = Some(dir);
}

fn now_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

fn actor(principal: &AuthPrincipal) -> Option<String> {
    principal.username.clone()
}

fn site(state: &AppState) -> weather::Site {
    let s = crate::launch_site::configured(state).unwrap_or(crate::launch_site::COMPILED_DEFAULT);
    weather::Site { lat_deg: s.lat_deg, lon_deg: s.lon_deg, alt_msl_m: s.alt_msl_m }
}

/// Accepted archive fires as fit points (the 2026-11-16 fire when the archive
/// has none).
fn fit_points(state: &AppState) -> Vec<motor::FitPoint> {
    fit_points_from(crate::fire_archive::fit_inputs(state))
}

fn fit_points_from(inputs: Vec<crate::fire_archive::FitInput>) -> Vec<motor::FitPoint> {
    let points: Vec<motor::FitPoint> = inputs
        .into_iter()
        .map(|f| motor::FitPoint {
            mean_pressure_pa: motor::mean_pressure_for_fire(f.tank_psi_at_ignition, f.supercharge_psi, f.ambient_c, f.fill_kg),
            id: f.id,
            label: f.label,
            fill_kg: f.fill_kg,
            impulse_ns: f.impulse_ns,
            impulse_sigma_ns: f.impulse_sigma_ns,
        })
        .collect();
    if points.is_empty() { vec![motor::FitPoint::reference()] } else { points }
}

fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn current_targets(state: &AppState) -> Targets {
    let t = state.fill_targets.lock().unwrap().nitrous.clone();
    Targets { nitrous_target_mass_kg: f64::from(t.target_mass_kg), nitrous_target_pressure_psi: f64::from(t.target_pressure_psi) }
}

/// The pressure limit for the supercharge target and where it came from.
fn pressure_limit(state: &AppState, settings: &Settings) -> (f64, Vec<String>) {
    let cfg = state.gse.lock().unwrap().engine.config.clone();
    let mut limit = settings.meop_psi;
    let mut sources = vec![format!("planner MEOP {:.0} psi", settings.meop_psi)];
    if let Some(c) = cfg.pressure_ceiling_psi {
        let v = f64::from(c - cfg.turbocharge_pressure_deadband_psi);
        sources.push(format!("GSE pressure ceiling {c:.0} psi minus the {:.0} psi turbocharge deadband", cfg.turbocharge_pressure_deadband_psi));
        limit = limit.min(v);
    }
    let relief = f64::from(cfg.overpressure_relief_psi) - 50.0;
    sources.push(format!("overpressure relief {:.0} psi minus 50 psi", cfg.overpressure_relief_psi));
    limit = limit.min(relief);
    if let Some(r) = cfg.tank_rating_psi {
        sources.push(format!("tank rating {r:.0} psi"));
        limit = limit.min(f64::from(r));
    }
    (limit, sources)
}

struct Prepared {
    input: solver::Input,
    vehicle_source: String,
    fit: motor::MotorFit,
    targets: Targets,
    cd_scale: f64,
    hash: String,
    weather_age_s: f64,
    settings: Settings,
    extra_warnings: Vec<String>,
}

fn prepare(state: &AppState) -> Result<Prepared, (StatusCode, String)> {
    let now = now_ms();
    let site = site(state);
    let points = fit_points(state);
    let fit = motor::MotorFit::fit(&points);
    let targets = current_targets(state);
    let inner = rt(state).inner.lock().unwrap();
    let settings = inner.settings.clone();
    let launch_ms = settings.launch_time_ms.unwrap_or(now);
    let mut extra = Vec::new();
    let snapshot = match settings.weather_source.as_str() {
        "manual" => settings.manual_weather.snapshot(site, now, launch_ms),
        _ => {
            let cached = inner.weather_cache.clone().ok_or((
                StatusCode::CONFLICT,
                "No forecast is cached: fetch one (online weather must be enabled) or use manual weather.".to_string(),
            ))?;
            let moved = (cached.site.lat_deg - site.lat_deg).abs() > 0.05 || (cached.site.lon_deg - site.lon_deg).abs() > 0.05;
            if moved {
                extra.push("The cached forecast is for a different launch site.".into());
            }
            let age_h = (now - cached.fetched_ms) as f64 / 3.6e6;
            if age_h > 6.0 {
                extra.push(format!("The forecast is {age_h:.1} h old."));
            }
            if ((cached.valid_time_ms - launch_ms) as f64).abs() / 3.6e6 > 3.0 {
                extra.push("The forecast hour is more than 3 h from the planned launch time.".into());
            }
            let mut s = cached;
            s.site.alt_msl_m = site.alt_msl_m;
            s
        }
    };
    let mut cfg = flight::Config::reference();
    let vehicle_source = match &inner.vehicle {
        Some(v) => {
            cfg.vehicle = v.imported.vehicle.clone();
            format!("OpenRocket import: {}", v.imported.name)
        }
        None => "tools/flight_profile/vehicle.json (assumed)".to_string(),
    };
    let cd_scale = inner.calibration.cd_scale();
    cfg.aero.cd_scale = cd_scale;
    let calibration_points = inner.calibration.points.len();
    let vehicle_ms = inner.vehicle.as_ref().map(|v| v.imported_ms);
    drop(inner);
    let (max_sc, sources) = pressure_limit(state, &settings);
    let tank_c = settings.tank_temp_c.unwrap_or(snapshot.surface_temp_c);
    let nominal = settings.nominal_supercharge_psi.unwrap_or(targets.nitrous_target_pressure_psi);
    let hash = fnv(&serde_json::to_string(&json!({
        "settings": settings,
        // Manual weather is part of the settings; a forecast is identified by its fetch time.
        "weather": if snapshot.provider == "manual" { 0 } else { snapshot.fetched_ms }, "weather_source": settings.weather_source,
        "vehicle": vehicle_ms, "calibration": calibration_points, "fit": points, "site": site,
        "max_sc": max_sc, "nominal": nominal,
    }))
    .unwrap_or_default());
    let weather_age_s = ((now - snapshot.fetched_ms).max(0) as f64) / 1000.0;
    let input = solver::Input {
        cfg,
        weather: snapshot,
        fit: fit.clone(),
        target_m: settings.target_apogee_m,
        tank_c,
        tank_volume_l: settings.tank_volume_l,
        min_ullage: settings.min_ullage_frac,
        min_fill_kg: settings.min_fill_kg,
        max_supercharge_psi: max_sc,
        limit_sources: sources,
        nominal_supercharge_psi: nominal,
        launch_ms,
        dt: 0.001,
    };
    Ok(Prepared { input, vehicle_source, fit, targets, cd_scale, hash, weather_age_s, settings, extra_warnings: extra })
}

/// Computes a plan from the current settings and weather (blocking: run off the async runtime).
pub(crate) fn compute(state: &AppState, principal: Option<&AuthPrincipal>) -> Result<Plan, (StatusCode, String)> {
    let p = prepare(state)?;
    let mut solution = solver::solve(&p.input);
    solution.warnings.extend(p.extra_warnings.iter().cloned());
    let now = now_ms();
    let fire_ids: Vec<String> = p.fit.fires.iter().map(|f| f.id.clone()).collect();
    let plan = Plan {
        id: format!("plan-{now}-{}", &p.hash[..8]),
        computed_ms: now,
        computed_by: principal.and_then(actor),
        solution,
        weather: p.input.weather.clone(),
        weather_age_s: p.weather_age_s,
        vehicle_source: p.vehicle_source,
        motor_fit: FitSummary { c_ref_ns_per_kg: p.fit.c_ref_ns_per_kg, n: p.fit.n, sigma_c_rel: p.fit.sigma_c_rel, fires: fire_ids.len(), fire_ids: fire_ids.clone() },
        current_targets: p.targets,
        cd_scale: p.cd_scale,
        tank_temp_c: p.input.tank_c,
        station_mode: crate::runtime_mode::label(crate::runtime_mode::current()).to_string(),
        inputs_hash: p.hash,
        settings: p.settings,
    };
    let mut inner = rt(state).inner.lock().unwrap();
    let changed = inner.fit_history.last().is_none_or(|h| {
        h.fire_ids != fire_ids || (h.c_ref_ns_per_kg - p.fit.c_ref_ns_per_kg).abs() > 1e-6 || (h.n - p.fit.n).abs() > 1e-9
    });
    if changed {
        inner.fit_history.push(FitHistory {
            at_ms: now,
            c_ref_ns_per_kg: p.fit.c_ref_ns_per_kg,
            n: p.fit.n,
            sigma_c_rel: p.fit.sigma_c_rel,
            fires: fire_ids.len(),
            fire_ids,
        });
        if let Some(dir) = inner.data_dir.clone() {
            let _ = write_json(&dir, FIT_HISTORY_FILE, &inner.fit_history);
        }
    }
    inner.last_plan = Some(plan.clone());
    Ok(plan)
}

/// Why an apply is refused now, or None.
pub(crate) fn apply_refusal(state: &AppState) -> Option<String> {
    use crate::station_mode::Mode;
    let mode = crate::runtime_mode::current();
    if matches!(mode, Mode::TestFire | Mode::Testing) {
        return Some(format!(
            "Apply is available only in Normal, Integrated ground test and HITL modes. {} mode plans no flight, so the planner never changes the fill targets here (compute is still available).",
            crate::runtime_mode::label(mode)
        ));
    }
    if crate::rrls::is_rrls() {
        return Some("Fill targets are set at the primary ground station.".into());
    }
    let flight = state.local_flight_state_snapshot();
    if !matches!(flight, FlightState::Idle | FlightState::PreFill) {
        return Some(format!("Fill targets can be set from a plan only before the fill starts (Idle or PreFill); the flight state is {flight:?}."));
    }
    let gse = state.gse.lock().unwrap();
    use gse_sequence::Phase as P;
    let fill_phase = matches!(gse.engine.status.phase, P::FillSetup | P::Filling | P::PauseSetup | P::Paused | P::Turbocharge);
    if gse.engine.active() || fill_phase {
        return Some("A GSE sequence (fill, turbocharge or test) is running: wait for it to finish.".into());
    }
    None
}

/// Writes the plan's fill targets through the manual-edit path.
pub(crate) fn apply(state: &AppState, principal: &AuthPrincipal, plan_id: &str) -> Result<Applied, (StatusCode, String)> {
    if principal.anonymous || !principal.permissions.send_commands {
        return Err((StatusCode::FORBIDDEN, "Applying a plan needs permission to send commands.".into()));
    }
    if let Some(reason) = apply_refusal(state) {
        return Err((StatusCode::CONFLICT, reason));
    }
    let plan = rt(state).inner.lock().unwrap().last_plan.clone().ok_or((StatusCode::CONFLICT, "Compute a plan first.".to_string()))?;
    if plan.id != plan_id {
        return Err((StatusCode::CONFLICT, "That plan is no longer the latest: review the new plan.".into()));
    }
    if plan.solution.status != "ok" {
        return Err((StatusCode::CONFLICT, "The target is not reachable; this plan cannot be applied.".into()));
    }
    let now = now_ms();
    if now - plan.computed_ms > PLAN_MAX_AGE_MS {
        return Err((StatusCode::CONFLICT, "The plan is more than 30 minutes old: compute it again.".into()));
    }
    let fresh = prepare(state)?;
    if fresh.hash != plan.inputs_hash {
        return Err((StatusCode::CONFLICT, "The settings, weather, vehicle or limits changed since this plan was computed: compute it again.".into()));
    }
    let previous = current_targets(state);
    let mut cfg = state.fill_targets.lock().unwrap().clone();
    cfg.nitrous.target_mass_kg = plan.solution.fill_mass_kg as f32;
    cfg.nitrous.target_pressure_psi = plan.solution.supercharge_psi as f32;
    let saved = crate::web::save_fill_targets(state, cfg)?;
    let applied = Applied {
        plan: plan.clone(),
        applied_ms: now,
        applied_by: actor(principal),
        previous_targets: previous,
        new_targets: Targets {
            nitrous_target_mass_kg: f64::from(saved.nitrous.target_mass_kg),
            nitrous_target_pressure_psi: f64::from(saved.nitrous.target_pressure_psi),
        },
    };
    let who = applied.applied_by.clone().unwrap_or_else(|| "unknown".into());
    let text = format!(
        "Apogee plan applied by {who}: N2O fill target {:.2} -> {:.2} kg, supercharge {:.0} -> {:.0} psi for {:.0} m ({:.0} ft) AGL; predicted {:.0} m ({:.0}..{:.0}), {} weather, plan {}.",
        previous.nitrous_target_mass_kg,
        applied.new_targets.nitrous_target_mass_kg,
        previous.nitrous_target_pressure_psi,
        applied.new_targets.nitrous_target_pressure_psi,
        plan.solution.target_apogee_m,
        plan.solution.target_apogee_m / FT,
        plan.solution.apogee_m,
        plan.solution.apogee_band_m[0],
        plan.solution.apogee_band_m[1],
        plan.weather.provider,
        plan.id,
    );
    log::info!("{text}");
    crate::web::persist_message_db_only(state, text);
    let mut inner = rt(state).inner.lock().unwrap();
    if let Some(dir) = inner.data_dir.clone() {
        let _ = write_json(&dir, APPLIED_FILE, &applied);
        let _ = append_log(&dir, &json!({"event": "applied", "at_ms": now, "by": applied.applied_by, "applied": applied}));
    }
    inner.applied = Some(applied.clone());
    Ok(applied)
}

/// For the launch capture manifest (`details.apogee_plan`).
pub(crate) fn capture_details(state: &AppState) -> Value {
    let inner = rt(state).inner.lock().unwrap();
    match &inner.applied {
        Some(a) => json!({
            "plan_id": a.plan.id,
            "applied_ms": a.applied_ms,
            "applied_by": a.applied_by,
            "target_apogee_m": a.plan.solution.target_apogee_m,
            "predicted_apogee_m": a.plan.solution.apogee_m,
            "apogee_band_m": a.plan.solution.apogee_band_m,
            "fill_mass_kg": a.new_targets.nitrous_target_mass_kg,
            "supercharge_psi": a.new_targets.nitrous_target_pressure_psi,
            "impulse_ns": a.plan.solution.impulse_ns,
            "impulse_sigma_ns": a.plan.solution.impulse_sigma_ns,
            "weather": a.plan.weather,
            "vehicle_source": a.plan.vehicle_source,
            "motor_fit": a.plan.motor_fit,
            "cd_scale": a.plan.cd_scale,
        }),
        None => Value::Null,
    }
}

/// The flight's apogee: the FC's own report, else the preferred GS maximum.
fn actual_apogee() -> Option<(f64, String)> {
    let m = crate::flight_maxima::snapshot();
    if let Some(e) = m.fc_altitude {
        return Some((e.value, e.source));
    }
    m.altitude.first().map(|e| (e.value, e.source.clone()))
}

fn post_flight(applied: Option<&Applied>) -> Value {
    let Some(a) = applied else { return Value::Null };
    let Some((actual, source)) = actual_apogee() else { return Value::Null };
    json!({
        "applied_plan_id": a.plan.id,
        "predicted_apogee_m": a.plan.solution.apogee_m,
        "band_m": a.plan.solution.apogee_band_m,
        "actual_apogee_m": actual,
        "source": source,
        "error_m": actual - a.plan.solution.apogee_m,
    })
}

/// Solves the drag scale that makes the applied plan reach `actual_m`.
fn solve_cd_scale(state: &AppState, plan: &Plan, actual_m: f64) -> Result<f64, String> {
    let mut cfg = flight::Config::reference();
    if let Some(v) = &rt(state).inner.lock().unwrap().vehicle {
        cfg.vehicle = v.imported.vehicle.clone();
    }
    let fit = motor::MotorFit::fit(&fit_points(state));
    let env = plan.weather.environment(cfg.environment.wind_shear_exponent, cfg.environment.wind_profile_cap);
    let m = motor::motor_for_fill(&cfg, &fit, plan.solution.fill_mass_kg, plan.tank_temp_c, plan.solution.supercharge_psi, plan.settings.tank_volume_l, 1.0);
    let apogee = |k: f64| {
        let mut c = cfg.clone();
        c.aero.cd_scale = k;
        flight::fly(&c, &env, &m.motor, 0.002).apogee_m
    };
    let (mut lo, mut hi) = (0.3, 3.0);
    if actual_m > apogee(lo) || actual_m < apogee(hi) {
        return Err("The actual apogee is outside what a drag scale of 0.3..3 explains: check the flight data.".into());
    }
    for _ in 0..30 {
        let mid = 0.5 * (lo + hi);
        if apogee(mid) > actual_m {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(0.5 * (lo + hi))
}

pub(crate) fn calibrate_flight(state: &AppState, principal: &AuthPrincipal, actual: Option<f64>) -> Result<Value, (StatusCode, String)> {
    if principal.anonymous || !principal.permissions.send_commands {
        return Err((StatusCode::FORBIDDEN, "Adding a calibration point needs permission to send commands.".into()));
    }
    let applied = rt(state).inner.lock().unwrap().applied.clone().ok_or((StatusCode::CONFLICT, "No plan has been applied.".to_string()))?;
    let actual = match actual {
        Some(a) if a.is_finite() && a > 10.0 => a,
        Some(_) => return Err((StatusCode::BAD_REQUEST, "The actual apogee must be a positive number of metres.".into())),
        None => actual_apogee().map(|a| a.0).ok_or((StatusCode::CONFLICT, "No flight apogee is known yet.".to_string()))?,
    };
    let k = solve_cd_scale(state, &applied.plan, actual).map_err(|e| (StatusCode::CONFLICT, e))?;
    let point = CalibrationPoint { at_ms: now_ms(), cd_scale: k, predicted_m: applied.plan.solution.apogee_m, actual_m: actual, plan_id: Some(applied.plan.id.clone()) };
    {
        let mut inner = rt(state).inner.lock().unwrap();
        inner.calibration.points.push(point.clone());
        if let Some(dir) = inner.data_dir.clone() {
            write_json(&dir, CALIBRATION_FILE, &inner.calibration)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("The calibration was not saved: {e}")))?;
            let _ = append_log(&dir, &json!({"event": "flight_calibration", "by": actor(principal), "point": point}));
        }
    }
    crate::web::persist_message_db_only(
        state,
        format!(
            "Apogee planner flight calibration added by {}: predicted {:.0} m, actual {actual:.0} m, drag scale {k:.3}.",
            actor(principal).unwrap_or_default(),
            point.predicted_m
        ),
    );
    Ok(view(state))
}

/// The snapshot as the firmware sim's plant `conditions`, clamped to its ranges.
fn firmware_sim_conditions(snap: &weather::Snapshot) -> Value {
    json!({
        "ambient_c": snap.surface_temp_c.clamp(-30.0, 50.0),
        "wind_mps": snap.wind_mps.clamp(0.0, 40.0),
        "wind_from_deg": snap.wind_from_deg.rem_euclid(360.0),
        "gust_mps": snap.gust_mps.clamp(0.0, 20.0),
    })
}

/// Sends the plan's weather to the firmware sim (admin; only when this station runs simulated firmware).
pub(crate) fn rehearse_in_firmware_sim(state: &AppState, principal: &AuthPrincipal) -> Result<Value, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Firmware simulation controls need an administrator.".into()));
    }
    let env = sim_environment(state);
    let control = crate::firmware_sim_mode::set_conditions(env["firmware_sim_conditions"].clone()).map_err(|e| (StatusCode::CONFLICT, e))?;
    crate::web::persist_message_db_only(
        state,
        format!("Apogee plan weather sent to the firmware simulation by {}: {}.", actor(principal).unwrap_or_default(), control["conditions"]),
    );
    Ok(json!({"control": control, "sim_environment": env}))
}

pub(crate) fn sim_environment(state: &AppState) -> Value {
    let inner = rt(state).inner.lock().unwrap();
    let snap = inner
        .applied
        .as_ref()
        .map(|a| a.plan.weather.clone())
        .or_else(|| inner.last_plan.as_ref().map(|p| p.weather.clone()))
        .or_else(|| inner.weather_cache.clone())
        .unwrap_or_else(|| inner.settings.manual_weather.snapshot(site(state), now_ms(), now_ms()));
    let fill = inner.applied.as_ref().map(|a| a.new_targets);
    json!({
        "environment": {
            "ambient_temp_c": snap.surface_temp_c,
            "surface_pressure_pa": snap.surface_pressure_pa,
            "wind_mps": snap.wind_mps,
            "wind_from_deg": snap.wind_from_deg,
            "gust_sigma_mps": snap.gust_mps,
            "winds_aloft": snap.levels.iter().map(|l| json!({"h_agl_m": l.h_agl_m, "speed_mps": l.speed_mps, "from_deg": l.from_deg})).collect::<Vec<_>>(),
        },
        "fill": fill.map(|t| json!({"nitrous_target_mass_kg": t.nitrous_target_mass_kg, "nitrous_target_pressure_psi": t.nitrous_target_pressure_psi})),
        "firmware_sim_conditions": firmware_sim_conditions(&snap),
        "source": snap.provider,
        "valid_time_ms": snap.valid_time_ms,
        "fetched_ms": snap.fetched_ms,
    })
}

pub(crate) fn view(state: &AppState) -> Value {
    let fit = motor::MotorFit::fit(&fit_points(state));
    let gate = apply_refusal(state);
    let site = site(state);
    let inner = rt(state).inner.lock().unwrap();
    let vehicle = match &inner.vehicle {
        Some(v) => json!({
            "source": "ork", "name": v.imported.name, "file_name": v.file_name,
            "dry_mass_kg": v.imported.vehicle.dry_mass_kg, "length_m": v.imported.vehicle.length_m,
            "diameter_m": v.imported.vehicle.diameter_m, "cn_alpha_per_rad": v.imported.vehicle.cn_alpha_per_rad,
            "static_margin_cal": v.imported.vehicle.static_margin_cal, "imported_ms": v.imported_ms,
            "warnings": v.imported.warnings, "components": v.imported.components,
        }),
        None => {
            let v = flight::Config::reference().vehicle;
            json!({
                "source": "default", "name": "tools/flight_profile/vehicle.json (assumed)",
                "dry_mass_kg": v.dry_mass_kg, "length_m": v.length_m, "diameter_m": v.diameter_m,
                "cn_alpha_per_rad": v.cn_alpha_per_rad, "static_margin_cal": v.static_margin_cal,
                "imported_ms": null, "warnings": [],
            })
        }
    };
    json!({
        "settings": inner.settings,
        "last_plan": inner.last_plan,
        "applied": inner.applied,
        "weather_cache": inner.weather_cache,
        "vehicle": vehicle,
        "motor_fit": {
            "c_ref_ns_per_kg": fit.c_ref_ns_per_kg, "n": fit.n, "sigma_c_rel": fit.sigma_c_rel,
            "sigma_n": fit.sigma_n, "p_ref_psi": fit.p_ref_psi, "fires": fit.fires,
            "history": inner.fit_history,
        },
        "apply_gate": {"allowed": gate.is_none(), "reason": gate},
        "post_flight": post_flight(inner.applied.as_ref()),
        "flight_calibration": {"cd_scale": inner.calibration.cd_scale(), "points": inner.calibration.points},
        "online_request_preview": weather::open_meteo_url(site),
    })
}

pub(crate) fn update_settings(state: &AppState, principal: &AuthPrincipal, settings: Settings) -> Result<Value, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Only an administrator may change the apogee planner settings.".into()));
    }
    settings.validate().map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    {
        let mut inner = rt(state).inner.lock().unwrap();
        if settings.weather_source == "open_meteo" && !settings.online_weather_enabled && inner.weather_cache.is_none() {
            return Err((StatusCode::BAD_REQUEST, "Enable online weather (or pick manual) to use the Open-Meteo forecast.".into()));
        }
        let enabled_now = settings.online_weather_enabled && !inner.settings.online_weather_enabled;
        if let Some(dir) = inner.data_dir.clone() {
            write_json(&dir, SETTINGS_FILE, &settings)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("The planner settings were not saved: {e}")))?;
        }
        inner.settings = settings;
        if enabled_now {
            let text = format!(
                "Online weather enabled for the apogee planner by {}: forecasts send the launch site rounded to 0.01° to api.open-meteo.com.",
                actor(principal).unwrap_or_default()
            );
            drop(inner);
            crate::web::persist_message_db_only(state, text);
        }
    }
    Ok(view(state))
}

pub(crate) fn import_ork(state: &AppState, principal: &AuthPrincipal, bytes: &[u8], file_name: Option<String>) -> Result<Value, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Only an administrator may import a vehicle.".into()));
    }
    if bytes.len() > MAX_ORK_BYTES {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "The .ork file is too large.".into()));
    }
    let imported = ork::import(bytes, &flight::Config::reference().vehicle).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let record = VehicleImport { imported_ms: now_ms(), file_name, imported };
    {
        let mut inner = rt(state).inner.lock().unwrap();
        if let Some(dir) = inner.data_dir.clone() {
            write_json(&dir, VEHICLE_FILE, &record)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("The vehicle was not saved: {e}")))?;
        }
        inner.vehicle = Some(record.clone());
    }
    crate::web::persist_message_db_only(
        state,
        format!(
            "Apogee planner vehicle imported from OpenRocket by {}: {} ({:.2} kg dry, {:.2} m, {:.2} cal).",
            actor(principal).unwrap_or_default(),
            record.imported.name,
            record.imported.vehicle.dry_mass_kg,
            record.imported.vehicle.length_m,
            record.imported.vehicle.static_margin_cal
        ),
    );
    Ok(view(state))
}

pub(crate) fn clear_ork(state: &AppState, principal: &AuthPrincipal) -> Result<Value, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Only an administrator may change the vehicle.".into()));
    }
    let mut inner = rt(state).inner.lock().unwrap();
    if let Some(dir) = inner.data_dir.clone() {
        let _ = std::fs::remove_file(dir.join(DIR).join(VEHICLE_FILE));
    }
    inner.vehicle = None;
    drop(inner);
    Ok(view(state))
}

fn store_weather(state: &AppState, snap: &weather::Snapshot) {
    let mut inner = rt(state).inner.lock().unwrap();
    if let Some(dir) = inner.data_dir.clone() {
        let _ = write_json(&dir, WEATHER_FILE, snap);
    }
    inner.weather_cache = Some(snap.clone());
}

// ------------------------------------------------------------ HTTP

fn error((status, text): (StatusCode, String)) -> Response {
    (status, Json(json!({"error": text}))).into_response()
}

async fn principal(state: &Arc<AppState>, headers: &HeaderMap, permission: Permission) -> Result<AuthPrincipal, Response> {
    crate::web::authorize_headers(state, headers, permission).await
}

async fn get_view(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = principal(&state, &headers, Permission::ViewData).await {
        return r;
    }
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(view(&state))).into_response()
}

async fn put_settings(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let p = match principal(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let settings: Settings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return error((StatusCode::BAD_REQUEST, format!("Invalid planner settings: {e}"))),
    };
    match update_settings(&state, &p, settings) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}

async fn post_fetch_weather(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let p = match principal(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if p.anonymous || !p.permissions.send_commands {
        return error((StatusCode::FORBIDDEN, "Fetching a forecast needs permission to send commands.".into()));
    }
    let (enabled, launch) = {
        let inner = rt(&state).inner.lock().unwrap();
        (inner.settings.online_weather_enabled, inner.settings.launch_time_ms)
    };
    if !enabled {
        return error((StatusCode::CONFLICT, "Online weather is off: an administrator must enable it in the planner settings.".into()));
    }
    let now = now_ms();
    match weather::fetch_open_meteo(site(&state), launch.unwrap_or(now), now).await {
        Ok(snap) => {
            store_weather(&state, &snap);
            Json(snap).into_response()
        }
        Err(e) => error((StatusCode::BAD_GATEWAY, format!("{e}. The cached forecast (if any) is still used."))),
    }
}

async fn post_compute(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let p = match principal(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if p.anonymous || !p.permissions.send_commands {
        return error((StatusCode::FORBIDDEN, "Computing a plan needs permission to send commands.".into()));
    }
    match tokio::task::spawn_blocking(move || compute(&state, Some(&p))).await {
        Ok(Ok(plan)) => Json(plan).into_response(),
        Ok(Err(e)) => error(e),
        Err(_) => error((StatusCode::INTERNAL_SERVER_ERROR, "The planner failed.".into())),
    }
}

#[derive(Deserialize)]
struct ApplyRequest {
    plan_id: String,
}

async fn post_apply(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let admission_generation = crate::gse::capture_operator_origin(&state).unwrap_or(0);
    let p = match principal(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let request: ApplyRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return error((StatusCode::BAD_REQUEST, "Expected {plan_id}.".into())),
    };
    let Some(_claim) = crate::gse::claim_operator_dispatch(&state, admission_generation) else {
        return crate::web::admission_refused(&state, "Applying the apogee plan");
    };
    match apply(&state, &p, &request.plan_id) {
        Ok(a) => Json(a).into_response(),
        Err(e) => error(e),
    }
}

async fn post_ork(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let p = match principal(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let name = headers.get("x-file-name").and_then(|v| v.to_str().ok()).map(|s| s.chars().take(120).collect());
    match tokio::task::spawn_blocking(move || import_ork(&state, &p, &body, name)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => error(e),
        Err(_) => error((StatusCode::INTERNAL_SERVER_ERROR, "The import failed.".into())),
    }
}

async fn delete_ork(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let p = match principal(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    match clear_ork(&state, &p) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}

#[derive(Deserialize, Default)]
struct CalibrateRequest {
    #[serde(default)]
    actual_apogee_m: Option<f64>,
}

async fn post_calibrate(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let p = match principal(&state, &headers, Permission::SendCommands).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let request: CalibrateRequest = if body.is_empty() { Default::default() } else { serde_json::from_slice(&body).unwrap_or_default() };
    match tokio::task::spawn_blocking(move || calibrate_flight(&state, &p, request.actual_apogee_m)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => error(e),
        Err(_) => error((StatusCode::INTERNAL_SERVER_ERROR, "The calibration failed.".into())),
    }
}

async fn post_rehearse(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let p = match principal(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    match rehearse_in_firmware_sim(&state, &p) {
        Ok(v) => Json(v).into_response(),
        Err(e) => error(e),
    }
}

async fn get_sim_environment(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = principal(&state, &headers, Permission::ViewData).await {
        return r;
    }
    Json(sim_environment(&state)).into_response()
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post, put};
    axum::Router::new()
        .route("/api/apogee_planner", get(get_view))
        .route("/api/apogee_planner/settings", put(put_settings))
        .route("/api/apogee_planner/weather/fetch", post(post_fetch_weather))
        .route("/api/apogee_planner/compute", post(post_compute))
        .route("/api/apogee_planner/apply", post(post_apply))
        .route(
            "/api/apogee_planner/ork",
            post(post_ork).delete(delete_ork).layer(DefaultBodyLimit::max(MAX_ORK_BYTES)),
        )
        .route("/api/apogee_planner/calibrate_flight", post(post_calibrate))
        .route("/api/apogee_planner/sim_environment", get(get_sim_environment))
        .route("/api/apogee_planner/sim_environment/apply", post(post_rehearse))
}
