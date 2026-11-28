//! Manual flight and burn simulations, kept as versioned "flight plan"
//! documents (docs/flight-plans.md).
//!
//! A plan flies the apogee planner's own models (crate::apogee_planner: the
//! flight model ported from tools/flight_profile, the hybrid or impulse-scaling
//! motor, the weather snapshot) with the operator's inputs. It records the
//! trajectory, events, thrust and burn curves and landing point, and a small
//! Monte Carlo landing dispersion. Plans are stored on the station, exported
//! as CSV/JSON and played in the replay engine as a predicted recording.
//!
//! Nothing here actuates anything or changes a setting: it is read-only
//! computation, allowed in every station mode.
use crate::apogee_planner::{self as planner, flight, hybrid, motor, solver, weather};
use crate::auth::{AuthPrincipal, Permission};
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const FORMAT: &str = "gs26.flight_plan";
pub(crate) const VERSION: u32 = 1;
const DIR: &str = "flight_plans";
pub(crate) const MAX_PLANS: usize = 50;
pub(crate) const MAX_SAMPLES: u32 = 500;
const DEFAULT_SAMPLES: u32 = 50;
/// Mean Earth radius, as the UI's replay scene uses.
pub(crate) const EARTH_RADIUS_M: f64 = 6_371_008.8;
/// Points sampled for an ascent / a descent (s).
const ASCENT_EVERY_S: f64 = 0.05;
const DESCENT_EVERY_S: f64 = 0.5;
/// χ² (2 dof) at 95 %: the ellipse scale is √5.991.
const CHI2_95: f64 = 5.991;

// ------------------------------------------------------------ geometry

/// Local tangent plane: ENU metres from an origin to latitude/longitude.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Origin {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
}

impl Origin {
    pub(crate) fn lat_lon(&self, e: f64, n: f64) -> (f64, f64) {
        let lat = self.lat_deg + (n / EARTH_RADIUS_M).to_degrees();
        let lon = self.lon_deg + (e / (EARTH_RADIUS_M * self.lat_deg.to_radians().cos().max(1e-6))).to_degrees();
        (lat, lon)
    }
    pub(crate) fn en(&self, lat: f64, lon: f64) -> (f64, f64) {
        let n = (lat - self.lat_deg).to_radians() * EARTH_RADIUS_M;
        let e = (lon - self.lon_deg).to_radians() * EARTH_RADIUS_M * self.lat_deg.to_radians().cos().max(1e-6);
        (e, n)
    }
}

pub(crate) fn round(x: f64, places: i32) -> f64 {
    let k = 10f64.powi(places);
    let r = (x * k).round() / k;
    if r == 0.0 { 0.0 } else { r }
}

/// Great-circle distance (m) and initial bearing (deg from north) from a to b.
pub(crate) fn distance_bearing(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> (f64, f64) {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    let d = 2.0 * EARTH_RADIUS_M * a.sqrt().atan2((1.0 - a).sqrt());
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (d, y.atan2(x).to_degrees().rem_euclid(360.0))
}

/// A 2D Gaussian summary of points (east, north m): mean, and the ellipse
/// holding `chi2` of the mass (semi-axes m, major axis bearing deg).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ellipse {
    pub e: f64,
    pub n: f64,
    pub semi_major_m: f64,
    pub semi_minor_m: f64,
    pub major_bearing_deg: f64,
}

impl Ellipse {
    pub(crate) fn of(points: &[(f64, f64)], chi2: f64) -> Option<Self> {
        if points.len() < 3 {
            return None;
        }
        let k = points.len() as f64;
        let (me, mn) = points.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0 / k, a.1 + p.1 / k));
        let (mut a, mut b, mut c) = (0.0, 0.0, 0.0);
        for p in points {
            let (de, dn) = (p.0 - me, p.1 - mn);
            a += de * de;
            b += de * dn;
            c += dn * dn;
        }
        let d = k - 1.0;
        Some(Self::from_covariance(me, mn, a / d, b / d, c / d, chi2))
    }
    /// From a covariance [[ee, en], [en, nn]].
    pub(crate) fn from_covariance(e: f64, n: f64, ee: f64, en: f64, nn: f64, chi2: f64) -> Self {
        let mid = (ee + nn) / 2.0;
        let r = (((ee - nn) / 2.0).powi(2) + en * en).sqrt();
        let (l1, l2) = ((mid + r).max(0.0), (mid - r).max(0.0));
        // Angle of the major axis from east, counter-clockwise.
        let theta = 0.5 * (2.0 * en).atan2(ee - nn);
        Self {
            e,
            n,
            semi_major_m: (l1 * chi2).sqrt(),
            semi_minor_m: (l2 * chi2).sqrt(),
            major_bearing_deg: (90.0 - theta.to_degrees()).rem_euclid(180.0),
        }
    }
    /// A closed ring (first point not repeated) of `k` points, [lat, lon].
    pub(crate) fn polygon(&self, origin: &Origin, k: usize) -> Vec<[f64; 2]> {
        let b = self.major_bearing_deg.to_radians();
        // Unit vectors (east, north) of the major and minor axes.
        let (ue, un) = (b.sin(), b.cos());
        let (ve, vn) = (un, -ue);
        (0..k)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * i as f64 / k as f64;
                let (x, y) = (self.semi_major_m * phi.cos(), self.semi_minor_m.max(0.5) * phi.sin());
                let (lat, lon) = origin.lat_lon(self.e + x * ue + y * ve, self.n + x * un + y * vn);
                [round(lat, 7), round(lon, 7)]
            })
            .collect()
    }
}

// ------------------------------------------------------------ random numbers

/// SplitMix64 with Box-Muller normals: small, deterministic per seed.
pub(crate) struct Rng(u64, Option<f64>);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15, None)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    pub(crate) fn normal(&mut self) -> f64 {
        if let Some(z) = self.1.take() {
            return z;
        }
        let (u1, u2) = (self.uniform(), self.uniform());
        let r = (-2.0 * u1.ln()).sqrt();
        let a = 2.0 * std::f64::consts::PI * u2;
        self.1 = Some(r * a.sin());
        r * a.cos()
    }
}

// ------------------------------------------------------------ request

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct WeatherRequest {
    /// `planner` (the planner's current source), `manual` or `cached`.
    pub source: Option<String>,
    pub manual: Option<weather::ManualWeather>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct LaunchRequest {
    pub rail_length_m: Option<f64>,
    pub tilt_deg: Option<f64>,
    pub azimuth_deg: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct DispersionRequest {
    pub samples: Option<u32>,
    pub seed: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct Request {
    /// `flight` (default) or `burn`.
    pub kind: Option<String>,
    pub label: Option<String>,
    pub fill_kg: Option<f64>,
    pub target_apogee_m: Option<f64>,
    pub supercharge_psi: Option<f64>,
    pub tank_temp_c: Option<f64>,
    pub motor_model: Option<String>,
    pub weather: Option<WeatherRequest>,
    pub launch: Option<LaunchRequest>,
    pub site: Option<weather::Site>,
    pub dispersion: Option<DispersionRequest>,
}

fn bad(text: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, text.into())
}

fn in_range(name: &str, v: Option<f64>, lo: f64, hi: f64, unit: &str) -> Result<(), (StatusCode, String)> {
    match v {
        Some(x) if !x.is_finite() || x < lo || x > hi => Err(bad(format!("{name} must be within {lo}..{hi} {unit}."))),
        _ => Ok(()),
    }
}

impl Request {
    fn validate(&self) -> Result<(), (StatusCode, String)> {
        if !matches!(self.kind.as_deref(), None | Some("flight") | Some("burn")) {
            return Err(bad("kind must be flight or burn."));
        }
        if self.label.as_ref().is_some_and(|l| l.chars().count() > 80) {
            return Err(bad("The label is limited to 80 characters."));
        }
        in_range("Fill", self.fill_kg, 0.1, 50.0, "kg")?;
        in_range("Target apogee", self.target_apogee_m, 30.0, 30_000.0, "m AGL")?;
        in_range("Supercharge", self.supercharge_psi, 0.0, 2000.0, "psi")?;
        in_range("Tank temperature", self.tank_temp_c, -40.0, 35.0, "°C")?;
        if !matches!(self.motor_model.as_deref(), None | Some("physics") | Some("scaling")) {
            return Err(bad("motor_model must be physics or scaling."));
        }
        if let Some(w) = &self.weather {
            if !matches!(w.source.as_deref(), None | Some("planner") | Some("manual") | Some("cached")) {
                return Err(bad("weather.source must be planner, manual or cached."));
            }
            if let Some(m) = &w.manual {
                m.validate().map_err(bad)?;
            }
        }
        if let Some(l) = &self.launch {
            in_range("Rail length", l.rail_length_m, 0.5, 30.0, "m")?;
            in_range("Rail tilt", l.tilt_deg, 0.0, 20.0, "°")?;
            in_range("Rail azimuth", l.azimuth_deg, 0.0, 360.0, "°")?;
        }
        if let Some(s) = &self.site {
            in_range("Site latitude", Some(s.lat_deg), -89.0, 89.0, "°")?;
            in_range("Site longitude", Some(s.lon_deg), -180.0, 180.0, "°")?;
            in_range("Site altitude", Some(s.alt_msl_m), -500.0, 6000.0, "m MSL")?;
        }
        if let Some(d) = &self.dispersion
            && d.samples.is_some_and(|n| n > MAX_SAMPLES)
        {
            return Err(bad(format!("At most {MAX_SAMPLES} dispersion samples.")));
        }
        Ok(())
    }
}

/// The request as it would run now: every field filled from the planner's
/// settings, the vehicle and the station's launch site.
pub(crate) fn defaults(state: &AppState) -> Request {
    let s = planner::settings_snapshot(state);
    let cfg = flight::Config::reference();
    Request {
        kind: Some("flight".into()),
        label: None,
        fill_kg: None,
        target_apogee_m: Some(s.target_apogee_m),
        supercharge_psi: s.nominal_supercharge_psi.or_else(|| Some(f64::from(state.fill_targets.lock().unwrap().nitrous.target_pressure_psi))),
        tank_temp_c: s.tank_temp_c,
        motor_model: Some(s.motor_model.clone()),
        weather: Some(WeatherRequest { source: Some("planner".into()), manual: Some(s.manual_weather.clone()) }),
        launch: Some(LaunchRequest { rail_length_m: Some(cfg.launch.rail_length_m), tilt_deg: Some(cfg.launch.tilt_deg), azimuth_deg: Some(cfg.launch.azimuth_deg) }),
        site: Some(planner::launch_site(state)),
        dispersion: Some(DispersionRequest { samples: Some(DEFAULT_SAMPLES), seed: Some(1) }),
    }
}

// ------------------------------------------------------------ plan

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Event {
    pub kind: String,
    pub t_s: f64,
    pub e_m: f64,
    pub n_m: f64,
    pub u_m: f64,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct Point {
    pub t_s: f64,
    pub phase: String,
    pub e_m: f64,
    pub n_m: f64,
    pub u_m: f64,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
    pub ve_mps: f64,
    pub vn_mps: f64,
    pub vu_mps: f64,
    pub attitude_q: Option<[f64; 4]>,
}

/// Inputs to a computation, resolved (what `run` flies).
pub(crate) struct Resolved {
    pub request: Request,
    pub kind: String,
    pub label: String,
    pub prepared: planner::Prepared,
    pub fill_kg: f64,
    pub supercharge_psi: f64,
    pub samples: u32,
    pub seed: u64,
    pub origin: Origin,
    pub warnings: Vec<String>,
}

pub(crate) fn resolve(state: &AppState, request: &Request, now_ms: i64) -> Result<Resolved, (StatusCode, String)> {
    request.validate()?;
    let mut settings = planner::settings_snapshot(state);
    if let Some(w) = &request.weather {
        match w.source.as_deref() {
            Some("manual") => {
                settings.weather_source = "manual".into();
                if let Some(m) = &w.manual {
                    settings.manual_weather = m.clone();
                }
            }
            Some("cached") => settings.weather_source = "cached".into(),
            _ => {}
        }
    }
    if let Some(t) = request.tank_temp_c {
        settings.tank_temp_c = Some(t);
    }
    if let Some(m) = &request.motor_model {
        settings.motor_model = m.clone();
    }
    if let Some(p) = request.supercharge_psi {
        settings.nominal_supercharge_psi = Some(p);
    }
    if let Some(t) = request.target_apogee_m {
        settings.target_apogee_m = t;
    }
    let overrides = planner::Overrides { settings, site: request.site, now_ms: Some(now_ms) };
    let mut prepared = planner::prepare_with(state, Some(&overrides))?;
    let mut warnings = std::mem::take(&mut prepared.extra_warnings);
    if let Some(l) = &request.launch {
        let launch = &mut prepared.input.cfg.launch;
        launch.rail_length_m = l.rail_length_m.unwrap_or(launch.rail_length_m);
        launch.tilt_deg = l.tilt_deg.unwrap_or(launch.tilt_deg);
        launch.azimuth_deg = l.azimuth_deg.unwrap_or(launch.azimuth_deg);
    }
    let kind = request.kind.clone().unwrap_or_else(|| "flight".into());
    let (fill_kg, supercharge_psi) = match request.fill_kg {
        Some(fill) => {
            let lim = solver::limits(&prepared.input);
            if fill > lim.max_fill_kg + 1e-9 {
                warnings.push(format!(
                    "The {fill:.2} kg fill is above the {:.2} kg the run tank holds with {:.0} % ullage at {:.0} °C.",
                    lim.max_fill_kg,
                    prepared.input.min_ullage * 100.0,
                    prepared.input.tank_c + solver::WARM_MARGIN_C
                ));
            }
            (fill, request.supercharge_psi.unwrap_or(prepared.input.nominal_supercharge_psi))
        }
        None => {
            let solution = solver::solve(&prepared.input);
            if solution.status != "ok" {
                warnings.push(format!("The planner cannot reach {:.0} m ({}): flying its closest fill.", solution.target_apogee_m, solution.status));
            }
            warnings.extend(solution.warnings.iter().cloned());
            (solution.fill_mass_kg, solution.supercharge_psi)
        }
    };
    let site = request.site.unwrap_or_else(|| planner::launch_site(state));
    let origin = Origin { lat_deg: site.lat_deg, lon_deg: site.lon_deg, alt_msl_m: prepared.input.weather.site.alt_msl_m };
    let dispersion = request.dispersion.clone().unwrap_or_default();
    let label = request.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| format!("{fill_kg:.2} kg, {supercharge_psi:.0} psi"));
    Ok(Resolved {
        request: request.clone(),
        kind,
        label,
        prepared,
        fill_kg,
        supercharge_psi,
        samples: if request.kind.as_deref() == Some("burn") { 0 } else { dispersion.samples.unwrap_or(DEFAULT_SAMPLES).min(MAX_SAMPLES) },
        seed: dispersion.seed.unwrap_or(1),
        origin,
        warnings,
    })
}

/// The motor a plan flies, and the hybrid burn it came from (physics motor).
fn build_motor(r: &Resolved) -> (motor::FillMotor, Option<(hybrid::Burn, f64)>) {
    let i = &r.prepared.input;
    match &i.physics {
        Some(pm) => {
            let (m, burn, k) = motor::physics_burn_for_fill(&i.cfg, pm, r.fill_kg, i.tank_c, r.supercharge_psi, i.tank_volume_l, 1.0);
            (m, Some((burn, k)))
        }
        None => (motor::motor_for_fill(&i.cfg, &i.fit, r.fill_kg, i.tank_c, r.supercharge_psi, i.tank_volume_l, 1.0), None),
    }
}

/// Burn curves (every 0.05 s), with the thrust scaled as the flight flies it.
fn burn_json(burn: &hybrid::Burn, k: f64) -> Value {
    const PSI: f64 = 6894.757;
    let end = burn.samples.last().map_or(0.0, |s| s.t);
    let ts = burn.times();
    let mut cols: [Vec<f64>; 8] = Default::default();
    let mut t = 0.0;
    let mut i = 0usize;
    while t <= end + 1e-9 {
        while i + 1 < burn.samples.len() && burn.samples[i + 1].t <= t {
            i += 1;
        }
        let s = &burn.samples[i];
        let n2o = hybrid::interp(&ts, &burn.samples.iter().map(|x| x.m_n2o).collect::<Vec<_>>(), t);
        let thrust = hybrid::interp(&ts, &burn.thrust(), t) * k;
        cols[0].push(round(t, 3));
        cols[1].push(round(thrust, 1));
        cols[2].push(round(s.pc / PSI - 14.696, 1).max(0.0));
        cols[3].push(round(s.p_tank / PSI - 14.696, 1));
        cols[4].push(round(s.mdot_ox, 4));
        cols[5].push(round(s.mdot_f, 4));
        cols[6].push(round(s.of, 3));
        cols[7].push(round(n2o, 3));
        t += 0.05;
    }
    let s = &burn.summary;
    json!({
        "t_s": cols[0], "thrust_n": cols[1], "chamber_psi": cols[2], "tank_psi": cols[3],
        "mdot_ox_kg_s": cols[4], "mdot_fuel_kg_s": cols[5], "of_ratio": cols[6], "n2o_kg": cols[7],
        "thrust_factor": round(k, 4),
        "summary": {
            "impulse_ns": round(s.impulse_ns * k, 1), "peak_thrust_n": round(s.peak_thrust_n * k, 1), "burn_time_s": round(s.burn_time_s, 3),
            "end_s": round(s.end_s, 3), "liquid_end_s": round(s.liquid_end_s, 3), "ox_used_kg": round(s.ox_used_kg, 3),
            "ox_left_kg": round(s.ox_left_kg, 3), "fuel_used_kg": round(s.fuel_used_kg, 3), "mean_of": round(s.mean_of, 3),
            "peak_chamber_psi": round(s.peak_pc_pa / PSI - 14.696, 1), "isp_s": round(s.isp_s, 1),
        },
    })
}

fn thrust_curve_json(m: &flight::Motor) -> Value {
    json!({
        "t_s": m.t.iter().map(|x| round(*x, 3)).collect::<Vec<_>>(),
        "thrust_n": m.f.iter().map(|x| round(*x, 1)).collect::<Vec<_>>(),
    })
}

/// One Monte Carlo flight: (apogee AGL, landing east, north).
fn sample_flight(cfg: &flight::Config, env: &flight::Environment, m: &flight::Motor, sig: &Sigmas, seed: u64, i: u32) -> (f64, f64, f64) {
    let mut rng = Rng::new(seed.wrapping_mul(0x2545_F491_4F6C_DD1D) ^ u64::from(i).wrapping_mul(0x9E37_79B9));
    let mut c = cfg.clone();
    let mut e = env.clone();
    e.wind = e.wind.scaled((1.0 + rng.normal() * sig.wind_speed_rel).max(0.0)).rotated(rng.normal() * sig.wind_dir_deg);
    let motor = m.thrust_scaled((1.0 + rng.normal() * sig.impulse_rel).max(0.3));
    c.aero.cd_scale *= (1.0 + rng.normal() * sig.cd_rel).max(0.5);
    c.recovery.drogue_descent_mps *= (1.0 + rng.normal() * sig.descent_rate_rel).max(0.5);
    c.recovery.main_descent_mps *= (1.0 + rng.normal() * sig.descent_rate_rel).max(0.5);
    let r = flight::fly(&c, &e, &motor, 0.002);
    let b = r.drift_bearing_deg.to_radians();
    (r.apogee_m, r.drift_m * b.sin(), r.drift_m * b.cos())
}

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Sigmas {
    pub wind_speed_rel: f64,
    pub wind_dir_deg: f64,
    pub impulse_rel: f64,
    pub cd_rel: f64,
    pub descent_rate_rel: f64,
}

/// Runs the Monte Carlo flights on the available cores (deterministic per seed).
fn monte_carlo(cfg: &flight::Config, env: &flight::Environment, m: &flight::Motor, sig: &Sigmas, samples: u32, seed: u64) -> Vec<(f64, f64, f64)> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 8).min(samples.max(1) as usize);
    let mut out = vec![(0.0, 0.0, 0.0); samples as usize];
    std::thread::scope(|scope| {
        for (k, chunk) in out.chunks_mut(samples.div_ceil(threads as u32).max(1) as usize).enumerate() {
            let base = k * samples.div_ceil(threads as u32).max(1) as usize;
            scope.spawn(move || {
                for (j, slot) in chunk.iter_mut().enumerate() {
                    *slot = sample_flight(cfg, env, m, sig, seed, (base + j) as u32);
                }
            });
        }
    });
    out
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let x = p * (sorted.len() - 1) as f64;
    let (i, f) = (x.floor() as usize, x - x.floor());
    sorted[i] + (sorted[(i + 1).min(sorted.len() - 1)] - sorted[i]) * f
}

pub(crate) const PHASE_LABELS: [(&str, &str); 6] =
    [("pad", "On the pad"), ("boost", "Powered ascent"), ("coast", "Coast"), ("drogue", "Drogue descent"), ("main", "Main descent"), ("landed", "Landed")];

/// MapOverlay tracks from a phase-labelled path ([lat, lon, alt]): one track per
/// run of the same phase, each joined to the next so the drawn line has no gap.
pub(crate) fn phase_tracks(path: &[(String, [f64; 3])], style: &str) -> Vec<Value> {
    let mut tracks: Vec<Value> = Vec::new();
    let mut i = 0usize;
    while i < path.len() {
        let phase = path[i].0.clone();
        let mut j = i;
        while j + 1 < path.len() && path[j + 1].0 == phase {
            j += 1;
        }
        let end = (j + 1).min(path.len() - 1);
        let pts: Vec<Value> = path[i..=end].iter().map(|(_, p)| json!(p)).collect();
        if pts.len() >= 2 {
            let label = PHASE_LABELS.iter().find(|(k, _)| *k == phase).map_or(phase.as_str(), |(_, l)| l);
            let n = tracks.iter().filter(|t| t["phase"] == phase.as_str()).count();
            let id = if n == 0 { phase.clone() } else { format!("{phase}-{n}") };
            tracks.push(json!({"id": id, "phase": phase, "label": label, "style": style, "points": pts}));
        }
        i = j + 1;
    }
    tracks
}

/// The plan's MapOverlay: one track per phase, event markers, the landing ellipse.
fn overlay(id: &str, label: &str, points: &[Point], events: &[Event], ellipse: Option<&Value>) -> Value {
    let path: Vec<(String, [f64; 3])> = points.iter().map(|p| (p.phase.clone(), [p.lat_deg, p.lon_deg, p.alt_msl_m])).collect();
    let tracks = phase_tracks(&path, "predicted");
    let markers: Vec<Value> = events
        .iter()
        .filter(|e| matches!(e.kind.as_str(), "apogee" | "drogue" | "main" | "landing"))
        .map(|e| {
            let (kind, text) = match e.kind.as_str() {
                "apogee" => ("apogee", format!("Predicted apogee {:.0} m AGL", e.u_m)),
                "landing" => ("landing", "Predicted landing".to_string()),
                "drogue" => ("event", "Predicted drogue".to_string()),
                _ => ("event", "Predicted main".to_string()),
            };
            json!({"id": e.kind, "kind": kind, "label": text, "lat_deg": e.lat_deg, "lon_deg": e.lon_deg, "alt_msl_m": e.alt_msl_m})
        })
        .collect();
    let areas: Vec<Value> = ellipse
        .map(|el| vec![json!({"id": "landing_ellipse", "kind": "landing_ellipse", "label": "Predicted 95% landing area", "polygon": el["polygon"]})])
        .unwrap_or_default();
    json!({
        "id": format!("flight_plan:{id}"), "kind": "flight_plan", "label": format!("Predicted: {label}"), "predicted": true,
        "tracks": tracks, "markers": markers, "areas": areas,
    })
}

/// Computes a plan (blocking: run off the async runtime). `now_ms` stamps it.
pub(crate) fn compute(state: &AppState, request: &Request, created_by: Option<String>, now_ms: i64) -> Result<Value, (StatusCode, String)> {
    let started = std::time::Instant::now();
    let r = resolve(state, request, now_ms)?;
    let (fm, burn) = build_motor(&r);
    let i = &r.prepared.input;
    let env = i.weather.environment(i.cfg.environment.wind_shear_exponent, i.cfg.environment.wind_profile_cap);
    let o = r.origin;
    let mut warnings = r.warnings.clone();
    let mut doc = json!({
        "format": FORMAT, "version": VERSION, "predicted": true,
        "kind": r.kind, "label": r.label, "created_ms": now_ms, "created_by": created_by,
        "station_mode": crate::runtime_mode::label(crate::runtime_mode::current()),
        "inputs": {
            "request": r.request,
            "fill_kg": r.fill_kg, "supercharge_psi": r.supercharge_psi, "tank_temp_c": round(i.tank_c, 2),
            "tank_volume_l": i.tank_volume_l, "launch": i.cfg.launch, "recovery": i.cfg.recovery,
            "weather": i.weather, "cd_scale": i.cfg.aero.cd_scale,
        },
        "vehicle_source": r.prepared.vehicle_source,
        "motor_model": if i.physics.is_some() { "physics" } else { "scaling" },
        "site": o,
        "thrust_curve": thrust_curve_json(&fm.motor),
        "burn": burn.as_ref().map(|(b, k)| burn_json(b, *k)),
    });
    let mut summary = json!({
        "fill_kg": r.fill_kg, "supercharge_psi": r.supercharge_psi, "tank_temp_c": round(i.tank_c, 2),
        "impulse_ns": round(fm.impulse_ns, 1), "impulse_sigma_rel": round(fm.sigma_rel, 4), "burn_s": round(fm.motor.burn_s(), 3),
    });
    let mut points: Vec<Point> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    let mut landing = Value::Null;
    let mut dispersion = Value::Null;
    if r.kind == "flight" {
        let mut trace = flight::Trace::new(ASCENT_EVERY_S, DESCENT_EVERY_S);
        let res = flight::fly_traced(&i.cfg, &env, &fm.motor, i.dt, Some(&mut trace));
        if res.liftoff_s <= 0.0 {
            warnings.push("The motor does not lift the vehicle off the rail.".into());
        }
        if res.rail_exit_mps > 0.0 && res.rail_exit_mps < solver::MIN_RAIL_EXIT_MPS {
            warnings.push(format!("Rail exit {:.1} m/s is below the {} m/s minimum.", res.rail_exit_mps, solver::MIN_RAIL_EXIT_MPS));
        }
        let at = |pos: [f64; 3]| {
            let (lat, lon) = o.lat_lon(pos[0], pos[1]);
            (round(lat, 7), round(lon, 7), round(o.alt_msl_m + pos[2], 2))
        };
        points = trace
            .points
            .iter()
            .map(|p| {
                let (lat, lon, alt) = at(p.pos);
                Point {
                    t_s: round(p.t, 3),
                    phase: p.phase.to_string(),
                    e_m: round(p.pos[0], 2),
                    n_m: round(p.pos[1], 2),
                    u_m: round(p.pos[2], 2),
                    lat_deg: lat,
                    lon_deg: lon,
                    alt_msl_m: alt,
                    ve_mps: round(p.vel[0], 2),
                    vn_mps: round(p.vel[1], 2),
                    vu_mps: round(p.vel[2], 2),
                    attitude_q: p.q.map(|q| q.map(|x| round(x, 5))),
                }
            })
            .collect();
        events = trace
            .events
            .iter()
            .map(|(kind, t, pos)| {
                let (lat, lon, alt) = at(*pos);
                Event { kind: kind.to_string(), t_s: round(*t, 3), e_m: round(pos[0], 2), n_m: round(pos[1], 2), u_m: round(pos[2], 2), lat_deg: lat, lon_deg: lon, alt_msl_m: alt }
            })
            .collect();
        let land = events.iter().find(|e| e.kind == "landing").cloned();
        if let Some(l) = &land {
            landing = json!({"lat_deg": l.lat_deg, "lon_deg": l.lon_deg, "alt_msl_m": l.alt_msl_m, "e_m": l.e_m, "n_m": l.n_m, "t_s": l.t_s});
        }
        summary["apogee_m"] = json!(round(res.apogee_m, 1));
        summary["apogee_msl_m"] = json!(round(o.alt_msl_m + res.apogee_m, 1));
        summary["time_to_apogee_s"] = json!(round(res.apogee_s - res.liftoff_s, 2));
        summary["liftoff_s"] = json!(round(res.liftoff_s, 3));
        summary["burnout_s"] = json!(round(res.burnout_s, 3));
        summary["max_speed_mps"] = json!(round(res.max_speed_mps, 1));
        summary["max_mach"] = json!(round(res.max_mach, 3));
        summary["rail_exit_mps"] = json!(round(res.rail_exit_mps, 2));
        summary["descent_s"] = json!(round(res.descent_s, 1));
        summary["landing_s"] = json!(land.as_ref().map(|l| l.t_s));
        summary["drift_m"] = json!(round(res.drift_m, 1));
        summary["drift_bearing_deg"] = json!(round(res.drift_bearing_deg, 1));
        if r.samples > 0 {
            let (_, sig_w) = i.weather.sigmas(i.launch_ms);
            let sig = Sigmas {
                wind_speed_rel: round((sig_w / i.weather.wind_mps.max(1.0)).min(0.5), 3),
                wind_dir_deg: if i.weather.provider == "manual" { 20.0 } else { 15.0 },
                impulse_rel: round(fm.sigma_rel.clamp(0.0, 0.5), 4),
                cd_rel: 0.05,
                descent_rate_rel: 0.08,
            };
            let mc_started = std::time::Instant::now();
            let runs = monte_carlo(&i.cfg, &env, &fm.motor, &sig, r.samples, r.seed);
            let mc_ms = mc_started.elapsed().as_secs_f64() * 1000.0;
            let mut apogees: Vec<f64> = runs.iter().map(|x| x.0).collect();
            apogees.sort_by(f64::total_cmp);
            let mean = apogees.iter().sum::<f64>() / apogees.len() as f64;
            let sd = (apogees.iter().map(|a| (a - mean).powi(2)).sum::<f64>() / (apogees.len().max(2) - 1) as f64).sqrt();
            let landings: Vec<(f64, f64)> = runs.iter().map(|x| (x.1, x.2)).collect();
            let ellipse = Ellipse::of(&landings, CHI2_95).map(|el| {
                let (lat, lon) = o.lat_lon(el.e, el.n);
                json!({
                    "center_lat_deg": round(lat, 7), "center_lon_deg": round(lon, 7), "center_e_m": round(el.e, 1), "center_n_m": round(el.n, 1),
                    "semi_major_m": round(el.semi_major_m, 1), "semi_minor_m": round(el.semi_minor_m, 1),
                    "major_bearing_deg": round(el.major_bearing_deg, 1), "confidence": 0.95, "polygon": el.polygon(&o, 48),
                })
            });
            dispersion = json!({
                "samples": r.samples, "seed": r.seed, "sigmas": sig, "compute_ms": round(mc_ms, 1),
                "apogee_m": {"mean": round(mean, 1), "sd": round(sd, 1), "p05": round(percentile(&apogees, 0.05), 1), "p95": round(percentile(&apogees, 0.95), 1)},
                "landing_ellipse": ellipse,
                "landing_points": landings.iter().map(|(e, n)| { let (a, b) = o.lat_lon(*e, *n); json!([round(a, 6), round(b, 6)]) }).collect::<Vec<_>>(),
            });
        }
    }
    let hash = fnv(&format!("{now_ms}{}", serde_json::to_string(&doc).unwrap_or_default()));
    let id = format!("fp-{now_ms}-{}", &hash[..8]);
    let ov = if r.kind == "flight" && !points.is_empty() { overlay(&id, &r.label, &points, &events, dispersion.get("landing_ellipse")) } else { Value::Null };
    doc["id"] = json!(id);
    doc["summary"] = summary;
    doc["events"] = json!(events);
    doc["points"] = json!(points);
    doc["landing"] = landing;
    doc["dispersion"] = dispersion;
    doc["warnings"] = json!(warnings);
    doc["overlay"] = ov;
    doc["compute_ms"] = json!(round(started.elapsed().as_secs_f64() * 1000.0, 1));
    Ok(doc)
}

fn fnv(text: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ------------------------------------------------------------ storage

pub(crate) fn directory(state: &AppState) -> PathBuf {
    Path::new(&state.placeholder_db_path).parent().unwrap_or(Path::new(".")).join(DIR)
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.starts_with("fp-") && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) fn save(dir: &Path, plan: &Value) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let id = plan["id"].as_str().unwrap_or_default();
    let path = dir.join(format!("{id}.json"));
    let tmp = path.with_extension("pending");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec(plan).unwrap_or_default())?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    // Keep the newest MAX_PLANS.
    let mut all = list_ids(dir);
    while all.len() > MAX_PLANS {
        let (oldest, _) = all.remove(0);
        log::info!("Flight plans: removed the oldest plan {oldest} (at most {MAX_PLANS} are kept).");
        let _ = std::fs::remove_file(dir.join(format!("{oldest}.json")));
    }
    Ok(())
}

/// Stored plan ids with their creation time, oldest first.
fn list_ids(dir: &Path) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name.strip_suffix(".json")?.to_string();
            let ms = id.split('-').nth(1)?.parse().ok()?;
            valid_id(&id).then_some((id, ms))
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    out
}

pub(crate) fn load(dir: &Path, id: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    serde_json::from_slice(&std::fs::read(dir.join(format!("{id}.json"))).ok()?).ok()
}

pub(crate) fn summary(plan: &Value) -> Value {
    json!({
        "id": plan["id"], "label": plan["label"], "kind": plan["kind"], "created_ms": plan["created_ms"], "created_by": plan["created_by"],
        "fill_kg": plan["summary"]["fill_kg"], "supercharge_psi": plan["summary"]["supercharge_psi"], "apogee_m": plan["summary"]["apogee_m"],
        "landing": if plan["landing"].is_object() { json!({
            "lat_deg": plan["landing"]["lat_deg"], "lon_deg": plan["landing"]["lon_deg"],
            "drift_m": plan["summary"]["drift_m"], "bearing_deg": plan["summary"]["drift_bearing_deg"],
        }) } else { Value::Null },
        "dispersion_samples": plan["dispersion"]["samples"].as_u64().unwrap_or(0),
        "warnings": plan["warnings"].as_array().map_or(0, Vec::len),
    })
}

/// Newest first.
pub(crate) fn list(dir: &Path) -> Vec<Value> {
    list_ids(dir).into_iter().rev().filter_map(|(id, _)| load(dir, &id)).collect()
}

// ------------------------------------------------------------ CSV

fn csv_num(v: &Value) -> String {
    match v.as_f64() {
        Some(x) => format!("{x}"),
        None => String::new(),
    }
}

/// The trajectory as CSV (one row per point; `event` names the events that
/// happened since the previous row), or the burn curves for a burn plan.
pub(crate) fn csv(plan: &Value) -> String {
    let mut out = String::new();
    if plan["kind"] == "burn" {
        let b = &plan["burn"];
        out.push_str("t_s,thrust_n,chamber_psi,tank_psi,mdot_ox_kg_s,mdot_fuel_kg_s,of_ratio,n2o_kg\n");
        let cols = ["t_s", "thrust_n", "chamber_psi", "tank_psi", "mdot_ox_kg_s", "mdot_fuel_kg_s", "of_ratio", "n2o_kg"];
        if b.is_object() {
            for i in 0..b["t_s"].as_array().map_or(0, Vec::len) {
                out.push_str(&cols.iter().map(|c| csv_num(&b[c][i])).collect::<Vec<_>>().join(","));
                out.push('\n');
            }
        } else {
            out.clear();
            out.push_str("t_s,thrust_n\n");
            let t = &plan["thrust_curve"];
            for i in 0..t["t_s"].as_array().map_or(0, Vec::len) {
                out.push_str(&format!("{},{}\n", csv_num(&t["t_s"][i]), csv_num(&t["thrust_n"][i])));
            }
        }
        return out;
    }
    out.push_str("t_s,phase,event,east_m,north_m,up_m,latitude_deg,longitude_deg,altitude_msl_m,v_east_mps,v_north_mps,v_up_mps,qw,qx,qy,qz\n");
    let events = plan["events"].as_array().cloned().unwrap_or_default();
    let mut prev_t = f64::NEG_INFINITY;
    for p in plan["points"].as_array().into_iter().flatten() {
        let t = p["t_s"].as_f64().unwrap_or(0.0);
        let names: Vec<&str> = events
            .iter()
            .filter(|e| e["t_s"].as_f64().is_some_and(|et| et > prev_t && et <= t + 1e-9))
            .filter_map(|e| e["kind"].as_str())
            .collect();
        prev_t = t;
        let q = &p["attitude_q"];
        let row = [
            csv_num(&p["t_s"]),
            p["phase"].as_str().unwrap_or_default().to_string(),
            names.join(" "),
            csv_num(&p["e_m"]),
            csv_num(&p["n_m"]),
            csv_num(&p["u_m"]),
            csv_num(&p["lat_deg"]),
            csv_num(&p["lon_deg"]),
            csv_num(&p["alt_msl_m"]),
            csv_num(&p["ve_mps"]),
            csv_num(&p["vn_mps"]),
            csv_num(&p["vu_mps"]),
            csv_num(&q[0]),
            csv_num(&q[1]),
            csv_num(&q[2]),
            csv_num(&q[3]),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

// ------------------------------------------------------------ replay

/// FlightState codes the replay rows use (crate::types::FlightState).
fn state_code(phase: &str) -> u8 {
    match phase {
        "pad" => 6,
        "boost" => 8,
        "coast" => 9,
        "drogue" => 11,
        "main" => 12,
        _ => 13,
    }
}

/// The plan as a portable replay document (docs/flight-plans.md): FC
/// `GPS_DATA`, `BAROMETER_DATA` and `FLIGHT_STATE` rows, ignition at
/// `created_ms`, 10 Hz in ascent and 2 Hz in descent.
pub(crate) fn replay_document(plan: &Value) -> Value {
    let t0 = plan["created_ms"].as_i64().unwrap_or(0);
    let site_alt = plan["site"]["alt_msl_m"].as_f64().unwrap_or(0.0);
    let p_sl = plan["inputs"]["weather"]["surface_pressure_pa"].as_f64();
    let temp_c = plan["inputs"]["weather"]["surface_temp_c"].as_f64().unwrap_or(15.0);
    let (dtk, psl) = flight::atmosphere_for_surface(site_alt, temp_c, p_sl);
    let mut samples = Vec::new();
    let mut id = 0u64;
    let mut push = |t_ms: i64, data_type: &str, values: Value, phase: Option<&str>| {
        id += 1;
        samples.push(json!({"id": id, "timestamp_ms": t_ms, "sender_id": "FC", "data_type": data_type, "values": values, "phase": phase, "source_timestamp_ms": null}));
    };
    let mut last_phase = String::new();
    let mut last_ms = i64::MIN;
    for p in plan["points"].as_array().into_iter().flatten() {
        let t_ms = t0 + (p["t_s"].as_f64().unwrap_or(0.0) * 1000.0).round() as i64;
        let phase = p["phase"].as_str().unwrap_or("pad");
        if phase != last_phase {
            let name = match phase {
                "pad" => "Armed",
                "boost" => "Ascent",
                "coast" => "Coast",
                "drogue" => "Descent",
                "main" => "Reefing",
                _ => "Landed",
            };
            push(t_ms, "FLIGHT_STATE", json!([state_code(phase)]), Some(name));
            last_phase = phase.to_string();
        }
        let step = if matches!(phase, "drogue" | "main") { 500 } else { 100 };
        if t_ms.saturating_sub(last_ms) < step && phase != "landed" {
            continue;
        }
        last_ms = t_ms;
        let alt_agl = p["u_m"].as_f64().unwrap_or(0.0);
        let (t_k, p_pa, ..) = flight::atmosphere(site_alt + alt_agl, dtk, psl);
        push(t_ms, "GPS_DATA", json!([p["lat_deg"], p["lon_deg"], p["alt_msl_m"]]), None);
        push(t_ms, "BAROMETER_DATA", json!([round(alt_agl, 2), round(p_pa, 1), round(t_k - 273.15, 2)]), None);
    }
    let end = samples.last().and_then(|s| s["timestamp_ms"].as_i64()).unwrap_or(t0);
    let label = format!("Predicted: {}", plan["label"].as_str().unwrap_or_default());
    json!({
        "format": "seds-replay", "version": 1, "predicted": true, "label": label,
        "recording": label, "mode": "flight",
        "configuration_provenance": "Flight plan computed by the station's flight model; predicted, not recorded.",
        "replay_context": {"recorded_mode": "normal", "provenance": "predicted", "predicted": true, "flight_plan": plan["id"], "start_ms": t0, "end_ms": end, "replay": true, "read_only": true},
        "snapshot": {"start_ms": t0, "end_ms": end, "upper_id": samples.len()},
        "samples": samples,
    })
}

/// The replay rows as the station's recording CSV columns.
pub(crate) fn replay_csv(replay: &Value) -> String {
    let mut out = String::from("id,received_timestamp_ms,source_timestamp_ms,sender_id,data_type,values_json,payload_json\n");
    for s in replay["samples"].as_array().into_iter().flatten() {
        let values = serde_json::to_string(&s["values"]).unwrap_or_default().replace('"', "\"\"");
        out.push_str(&format!("{},{},,\"FC\",\"{}\",\"{}\",\"[]\"\n", s["id"], s["timestamp_ms"], s["data_type"].as_str().unwrap_or_default(), values));
    }
    out
}

/// The track as the recording trajectory route returns it.
pub(crate) fn replay_trajectory(replay: &Value) -> Value {
    let mut first = true;
    let points: Vec<Value> = replay["samples"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["data_type"] == "GPS_DATA")
        .map(|s| {
            let v = json!({"timestamp_ms": s["timestamp_ms"], "latitude": s["values"][0], "longitude": s["values"][1], "altitude_m": s["values"][2], "break_before": first});
            first = false;
            v
        })
        .collect();
    json!({"upper_id": replay["snapshot"]["upper_id"], "source_sender_id": "FC", "points": points, "truncated": false, "sampled": false})
}

// ------------------------------------------------------------ HTTP

fn error((status, text): (StatusCode, String)) -> Response {
    (status, Json(json!({"error": text}))).into_response()
}

fn wall_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

fn may_run(p: &AuthPrincipal) -> bool {
    !p.anonymous && p.permissions.send_commands
}

async fn get_list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return r;
    }
    let dir = directory(&state);
    let st = state.clone();
    let (plans, defaults) = tokio::task::spawn_blocking(move || (list(&dir).iter().map(summary).collect::<Vec<_>>(), defaults(&st))).await.unwrap_or_default();
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"plans": plans, "defaults": defaults, "limits": {"max_samples": MAX_SAMPLES, "max_plans": MAX_PLANS}})),
    )
        .into_response()
}

async fn post_run(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let p = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !may_run(&p) {
        return error((StatusCode::FORBIDDEN, "Running a flight simulation needs permission to send commands (operator or administrator).".into()));
    }
    let request: Request = if body.is_empty() {
        Request::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return error(bad(format!("Invalid flight simulation request: {e}"))),
        }
    };
    let st = state.clone();
    let who = p.username.clone();
    let result = tokio::task::spawn_blocking(move || {
        let plan = compute(&st, &request, who, wall_ms())?;
        save(&directory(&st), &plan).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("The plan was not saved: {e}")))?;
        Ok::<Value, (StatusCode, String)>(plan)
    })
    .await;
    match result {
        Ok(Ok(plan)) => {
            log::info!(
                "Flight plan {} computed by {}: {} kg, apogee {} m, {} dispersion samples in {} ms.",
                plan["id"].as_str().unwrap_or_default(),
                p.username.as_deref().unwrap_or("unknown"),
                plan["summary"]["fill_kg"],
                plan["summary"]["apogee_m"],
                plan["dispersion"]["samples"],
                plan["compute_ms"]
            );
            Json(plan).into_response()
        }
        Ok(Err(e)) => error(e),
        Err(_) => error((StatusCode::INTERNAL_SERVER_ERROR, "The flight simulation failed.".into())),
    }
}

async fn read_plan(state: &Arc<AppState>, headers: &HeaderMap, id: &str) -> Result<Value, Response> {
    crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    if !valid_id(id) {
        return Err(error(bad("Invalid plan id.")));
    }
    let dir = directory(state);
    let id = id.to_string();
    tokio::task::spawn_blocking(move || load(&dir, &id))
        .await
        .ok()
        .flatten()
        .ok_or_else(|| error((StatusCode::NOT_FOUND, "No such flight plan.".into())))
}

async fn get_plan(State(state): State<Arc<AppState>>, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    match read_plan(&state, &headers, &id).await {
        Ok(plan) => Json(plan).into_response(),
        Err(r) => r,
    }
}

async fn delete_plan(State(state): State<Arc<AppState>>, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    let p = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !may_run(&p) {
        return error((StatusCode::FORBIDDEN, "Deleting a flight plan needs permission to send commands.".into()));
    }
    if !valid_id(&id) {
        return error(bad("Invalid plan id."));
    }
    match std::fs::remove_file(directory(&state).join(format!("{id}.json"))) {
        Ok(()) => Json(json!({"deleted": id})).into_response(),
        Err(_) => error((StatusCode::NOT_FOUND, "No such flight plan.".into())),
    }
}

async fn get_csv(State(state): State<Arc<AppState>>, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    match read_plan(&state, &headers, &id).await {
        Ok(plan) => (
            [(header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}.csv\""))],
            csv(&plan),
        )
            .into_response(),
        Err(r) => r,
    }
}

async fn get_json(State(state): State<Arc<AppState>>, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    match read_plan(&state, &headers, &id).await {
        Ok(plan) => (
            [(header::CONTENT_TYPE, "application/json".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}.json\""))],
            serde_json::to_vec_pretty(&plan).unwrap_or_default(),
        )
            .into_response(),
        Err(r) => r,
    }
}

async fn get_bundle(State(state): State<Arc<AppState>>, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    let plan = match read_plan(&state, &headers, &id).await {
        Ok(plan) => plan,
        Err(r) => return r,
    };
    if plan["kind"] != "flight" || plan["points"].as_array().is_none_or(Vec::is_empty) {
        return error((StatusCode::CONFLICT, "A burn plan has no trajectory to replay.".into()));
    }
    match crate::recording_bundle::build_plan(state, &headers, &plan, wall_ms()).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, crate::recording_bundle::MEDIA_TYPE.to_owned()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{id}.gsreplay\"")),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => error(e),
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/flight_plans", get(get_list))
        .route("/api/flight_plans/run", post(post_run))
        .route("/api/flight_plans/{id}", get(get_plan).delete(delete_plan))
        .route("/api/flight_plans/{id}/csv", get(get_csv))
        .route("/api/flight_plans/{id}/json", get(get_json))
        .route("/api/flight_plans/{id}/bundle", get(get_bundle))
}

#[cfg(test)]
#[path = "flight_plan_tests.rs"]
mod tests;
