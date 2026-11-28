//! Loss-of-signal drift prediction for recovery (docs/flight-plans.md,
//! "Loss-of-signal prediction").
//!
//! When the rocket's packets stop in flight, the station keeps flying the last
//! known state forward with the apogee planner's models: a point-mass coast to
//! apogee when it was still climbing, then the drogue and main descents with the
//! recovery rates of the vehicle and the winds of the weather the station
//! flies (manual entry, the cached forecast, works offline), down to the launch
//! site's ground altitude. A small Monte Carlo over the wind, the descent rates
//! and the last fix gives the uncertainty, which grows with time.
//!
//! It is advisory only ("Predicted: no signal"): nothing here commands
//! anything. It stops when a packet or GPS fix comes back. The prediction for
//! the time of that fix is then compared with the fix, and the error is logged.
use crate::apogee_planner::{self as planner, flight};
use crate::auth::Permission;
use crate::flight_plan::{Ellipse, Origin, Rng, distance_bearing, round};
use crate::state::AppState;
use crate::types::{FlightState, TelemetryRow};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub(crate) const LABEL: &str = "Predicted: no signal";
const DIR: &str = "los_prediction";
const SETTINGS_FILE: &str = "settings.json";
const LOG_FILE: &str = "comparisons.jsonl";
/// Monte Carlo descents per prediction.
pub(crate) const SAMPLES: u32 = 48;
const CHI2_95: f64 = 5.991;
/// 1σ errors of the last fix: GPS position (m) and the velocity estimate (m/s).
const FIX_SIGMA_M: f64 = 5.0;
const VEL_SIGMA_MPS: f64 = 3.0;
const DESCENT_RATE_SIGMA: f64 = 0.10;
/// A GPS fix after the signal returns is compared for this long.
const COMPARE_WINDOW_MS: i64 = 60_000;
const HISTORY: usize = 10;

// ------------------------------------------------------------ model

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Boost,
    Coast,
    Drogue,
    Main,
}

impl Phase {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "boost" => Some(Self::Boost),
            "coast" => Some(Self::Coast),
            "drogue" => Some(Self::Drogue),
            "main" => Some(Self::Main),
            _ => None,
        }
    }
    /// The flight phase a station flight state implies (None: not in flight or unknown).
    pub(crate) fn of_state(s: FlightState) -> Option<Self> {
        match s {
            FlightState::Launch | FlightState::Ascent => Some(Self::Boost),
            FlightState::Coast | FlightState::Apogee => Some(Self::Coast),
            FlightState::Descent => Some(Self::Drogue),
            FlightState::Reefing => Some(Self::Main),
            _ => None,
        }
    }
}

/// The last known state: receipt time, position and the velocity estimate (ENU).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LastFix {
    pub t_ms: i64,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
    #[serde(default)]
    pub ve_mps: Option<f64>,
    #[serde(default)]
    pub vn_mps: Option<f64>,
    #[serde(default)]
    pub vu_mps: Option<f64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// What a prediction flies: the vehicle and recovery, the atmosphere and wind,
/// the ground (the launch site) and the wind uncertainty.
#[derive(Clone)]
pub(crate) struct Model {
    pub cfg: flight::Config,
    pub env: flight::Environment,
    pub origin: Origin,
    pub wind_source: String,
    pub wind_speed_rel: f64,
    pub wind_dir_deg: f64,
}

impl Model {
    pub(crate) fn from_weather(snap: &planner::weather::Snapshot, site: planner::weather::Site) -> Self {
        let cfg = flight::Config::reference();
        let mut snap = snap.clone();
        snap.site.alt_msl_m = site.alt_msl_m;
        let env = snap.environment(cfg.environment.wind_shear_exponent, cfg.environment.wind_profile_cap);
        let (_, sig_w) = snap.sigmas(snap.valid_time_ms);
        Self {
            cfg,
            env,
            origin: Origin { lat_deg: site.lat_deg, lon_deg: site.lon_deg, alt_msl_m: site.alt_msl_m },
            wind_source: snap.provider.clone(),
            wind_speed_rel: (sig_w / snap.wind_mps.max(1.0)).min(0.5),
            wind_dir_deg: if snap.provider == "manual" { 20.0 } else { 15.0 },
        }
    }
    pub(crate) fn current(state: &AppState) -> Self {
        Self::from_weather(&planner::current_weather(state), planner::launch_site(state))
    }
}

/// One flown path from the last fix: points (t from the fix, ENU, phase).
fn fly_from(m: &Model, env: &flight::Environment, scale: flight::DescentScale, start: [f64; 3], vel: [f64; 3], phase: Option<Phase>) -> flight::Trace {
    let cfg = &m.cfg;
    let mut tr = flight::Trace::new(0.5, 1.0);
    let main_alt = cfg.recovery.main_deploy_alt_agl_m;
    let climbing = vel[2] > 1.0 && !matches!(phase, Some(Phase::Drogue | Phase::Main));
    let (e, n, h, t, drogue_in, main_open) = if climbing {
        let mass = cfg.vehicle.dry_mass_kg + cfg.motor.propellant_mass_kg * cfg.motor.residual_fraction;
        let (p, _, t) = coast_traced(cfg, env, mass, start, vel, &mut tr);
        (p[0], p[1], p[2], t, Some(cfg.recovery.drogue_delay_after_apogee_s), false)
    } else {
        match phase {
            Some(Phase::Main) => (start[0], start[1], start[2], 0.0, None, true),
            Some(Phase::Drogue) => (start[0], start[1], start[2], 0.0, None, false),
            // Around apogee: the drogue is still to come.
            Some(Phase::Coast | Phase::Boost) => (start[0], start[1], start[2], 0.0, Some(cfg.recovery.drogue_delay_after_apogee_s), false),
            // Unknown phase, descending: under the drogue above the main altitude, the main below.
            None => (start[0], start[1], start[2], 0.0, None, start[2] <= main_alt),
        }
    };
    flight::descend(cfg, env, flight::DescentStart { e, n, h: h.max(0.0), t, drogue_in_s: drogue_in, main_open }, scale, 0.25, Some(&mut tr));
    tr
}

/// The coast to apogee with points every 0.5 s.
fn coast_traced(cfg: &flight::Config, env: &flight::Environment, mass: f64, start: [f64; 3], vel: [f64; 3], tr: &mut flight::Trace) -> ([f64; 3], [f64; 3], f64) {
    let (mut p, mut v, mut t) = (start, vel, 0.0);
    tr.points.push(flight::TracePoint { t, pos: p, vel: v, q: None, phase: "coast" });
    while v[2] > 0.0 && t < 300.0 {
        let (np, nv, dt) = flight::coast_to_apogee(cfg, env, mass, p, v, 0.02, 0.5);
        p = np;
        v = nv;
        t += dt;
        tr.points.push(flight::TracePoint { t, pos: p, vel: v, q: None, phase: "coast" });
        if dt < 0.5 - 1e-9 {
            break;
        }
    }
    tr.events.push(("apogee", t, p));
    (p, v, t)
}

/// Position and phase at `t` along a trace (the landing point after it lands).
fn at(tr: &flight::Trace, t: f64) -> ([f64; 3], &'static str) {
    let pts = &tr.points;
    if pts.is_empty() {
        return ([0.0; 3], "landed");
    }
    if t <= pts[0].t {
        return (pts[0].pos, pts[0].phase);
    }
    for w in pts.windows(2) {
        if t <= w[1].t {
            let k = if w[1].t > w[0].t { (t - w[0].t) / (w[1].t - w[0].t) } else { 1.0 };
            let p = std::array::from_fn(|i| w[0].pos[i] + (w[1].pos[i] - w[0].pos[i]) * k);
            return (p, w[0].phase);
        }
    }
    let last = &pts[pts.len() - 1];
    (last.pos, "landed")
}

/// A prediction `elapsed_s` after the last fix.
#[derive(Clone, Debug)]
pub(crate) struct Prediction {
    pub phase_now: &'static str,
    pub now_enu: [f64; 3],
    pub now_ellipse: Option<Ellipse>,
    pub landing_en: (f64, f64),
    pub landing_in_s: f64,
    pub landing_ellipse: Option<Ellipse>,
    /// (phase, ENU) along the nominal path from the fix to the landing.
    pub path: Vec<(&'static str, [f64; 3])>,
}

/// The velocity to fly from: the estimate, else a descent at the vehicle's
/// rate for the phase (horizontal velocity is the wind's under a canopy).
fn start_velocity(m: &Model, fix: &LastFix, phase: Option<Phase>) -> [f64; 3] {
    let r = &m.cfg.recovery;
    let default_vu = match phase {
        Some(Phase::Boost) | Some(Phase::Coast) => 0.0,
        Some(Phase::Main) => -r.main_descent_mps,
        _ => -r.drogue_descent_mps,
    };
    [fix.ve_mps.unwrap_or(0.0), fix.vn_mps.unwrap_or(0.0), fix.vu_mps.unwrap_or(default_vu)]
}

pub(crate) fn predict(m: &Model, fix: &LastFix, phase: Option<Phase>, elapsed_s: f64, samples: u32, seed: u64) -> Prediction {
    let (e0, n0) = m.origin.en(fix.lat_deg, fix.lon_deg);
    let start = [e0, n0, (fix.alt_msl_m - m.origin.alt_msl_m).max(0.0)];
    let vel = start_velocity(m, fix, phase);
    let nominal = fly_from(m, &m.env, flight::DescentScale::default(), start, vel, phase);
    let (now, phase_now) = at(&nominal, elapsed_s);
    let land = nominal.points.last().map_or(start, |p| p.pos);
    let landing_t = nominal.points.last().map_or(0.0, |p| p.t);
    let mut nows = Vec::with_capacity(samples as usize);
    let mut lands = Vec::with_capacity(samples as usize);
    for i in 0..samples {
        let mut rng = Rng::new(seed ^ u64::from(i).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut env = m.env.clone();
        env.wind = env.wind.scaled((1.0 + rng.normal() * m.wind_speed_rel).max(0.0)).rotated(rng.normal() * m.wind_dir_deg);
        let scale = flight::DescentScale { drogue_rate: (1.0 + rng.normal() * DESCENT_RATE_SIGMA).max(0.5), main_rate: (1.0 + rng.normal() * DESCENT_RATE_SIGMA).max(0.5) };
        let s = [start[0] + rng.normal() * FIX_SIGMA_M, start[1] + rng.normal() * FIX_SIGMA_M, (start[2] + rng.normal() * FIX_SIGMA_M).max(0.0)];
        let v = [vel[0] + rng.normal() * VEL_SIGMA_MPS, vel[1] + rng.normal() * VEL_SIGMA_MPS, vel[2] + rng.normal() * VEL_SIGMA_MPS];
        let tr = fly_from(m, &env, scale, s, v, phase);
        let (p, _) = at(&tr, elapsed_s);
        nows.push((p[0], p[1]));
        let l = tr.points.last().map_or(s, |x| x.pos);
        lands.push((l[0], l[1]));
    }
    let path = nominal.points.iter().map(|p| (p.phase, p.pos)).collect();
    Prediction {
        phase_now: if elapsed_s >= landing_t { "landed" } else { phase_now },
        now_enu: now,
        now_ellipse: Ellipse::of(&nows, CHI2_95),
        landing_en: (land[0], land[1]),
        landing_in_s: (landing_t - elapsed_s).max(0.0),
        landing_ellipse: Ellipse::of(&lands, CHI2_95),
        path,
    }
}

/// The prediction as the route's fields and its MapOverlay.
pub(crate) fn prediction_json(m: &Model, fix: &LastFix, p: &Prediction, fix_age_s: f64, station: Option<(f64, f64)>) -> Value {
    let o = &m.origin;
    let ll = |e: f64, n: f64| {
        let (a, b) = o.lat_lon(e, n);
        (round(a, 7), round(b, 7))
    };
    let (nlat, nlon) = ll(p.now_enu[0], p.now_enu[1]);
    let (llat, llon) = ll(p.landing_en.0, p.landing_en.1);
    let now_alt = round(o.alt_msl_m + p.now_enu[2], 1);
    let sigma_now = p.now_ellipse.map(|el| round(el.semi_major_m / CHI2_95.sqrt(), 1));
    let mut path: Vec<(String, [f64; 3])> = vec![("coast".into(), [fix.lat_deg, fix.lon_deg, fix.alt_msl_m])];
    path.extend(p.path.iter().map(|(ph, pos)| {
        let (a, b) = ll(pos[0], pos[1]);
        (ph.to_string(), [a, b, round(o.alt_msl_m + pos[2], 1)])
    }));
    if let Some(first) = p.path.first() {
        path[0].0 = first.0.to_string();
    }
    let mut areas = Vec::new();
    if let Some(el) = &p.now_ellipse {
        areas.push(json!({"id": "position_uncertainty", "kind": "position_uncertainty", "label": format!("{LABEL}: 95% position area"), "polygon": el.polygon(o, 36)}));
    }
    if let Some(el) = &p.landing_ellipse {
        areas.push(json!({"id": "landing_ellipse", "kind": "landing_ellipse", "label": format!("{LABEL}: 95% landing area"), "polygon": el.polygon(o, 48)}));
    }
    let overlay = json!({
        "id": "los_prediction", "kind": "los_prediction", "label": LABEL, "predicted": true,
        "tracks": crate::flight_plan::phase_tracks(&path, "predicted"),
        "markers": [
            {"id": "last_fix", "kind": "last_fix", "label": format!("Last fix {} ago", crate::watch::format_age((fix_age_s * 1000.0) as i64)), "lat_deg": fix.lat_deg, "lon_deg": fix.lon_deg, "alt_msl_m": fix.alt_msl_m},
            {"id": "predicted_position", "kind": "predicted_position", "label": format!("{LABEL}: now"), "lat_deg": nlat, "lon_deg": nlon, "alt_msl_m": now_alt},
            {"id": "landing", "kind": "landing", "label": format!("{LABEL}: landing"), "lat_deg": llat, "lon_deg": llon, "alt_msl_m": round(o.alt_msl_m, 1)},
        ],
        "areas": areas,
    });
    let from_station = station.map(|(slat, slon)| {
        let (d, b) = distance_bearing(slat, slon, nlat, nlon);
        let (dl, bl) = distance_bearing(slat, slon, llat, llon);
        json!({"bearing_deg": round(b, 1), "distance_m": round(d, 0), "landing_bearing_deg": round(bl, 1), "landing_distance_m": round(dl, 0)})
    });
    json!({
        "phase": p.phase_now,
        "predicted_now": {"lat_deg": nlat, "lon_deg": nlon, "alt_msl_m": now_alt, "alt_agl_m": round(p.now_enu[2], 1), "sigma_m": sigma_now},
        "landing": {
            "lat_deg": llat, "lon_deg": llon, "alt_msl_m": round(o.alt_msl_m, 1), "in_s": round(p.landing_in_s, 1),
            "t_ms": fix.t_ms + ((fix_age_s + p.landing_in_s) * 1000.0) as i64,
            "semi_major_m": p.landing_ellipse.map(|e| round(e.semi_major_m, 1)), "semi_minor_m": p.landing_ellipse.map(|e| round(e.semi_minor_m, 1)),
        },
        "station": station.map(|(a, b)| json!({"lat_deg": a, "lon_deg": b})),
        "from_station": from_station,
        "wind_source": m.wind_source,
        "overlay": overlay,
    })
}

// ------------------------------------------------------------ live state

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub timeout_s: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, timeout_s: 5.0 }
    }
}

#[derive(Clone, Copy, Debug)]
struct Gps {
    t_ms: i64,
    lat: f64,
    lon: f64,
    alt: f64,
    rf: bool,
}

#[derive(Default)]
struct Inner {
    data_dir: Option<PathBuf>,
    settings: Settings,
    /// Recent rocket GPS fixes (receipt time), newest last.
    fixes: VecDeque<Gps>,
    /// Newest FC/RF packet (receipt ms).
    last_packet_ms: Option<i64>,
    /// While predicting: the fix flown from, the phase, the loss time.
    predicting: Option<(LastFix, Option<Phase>, i64)>,
    last_view: Option<Value>,
    /// After the signal came back: the fix to compare with (once one arrives).
    resolved: Option<(LastFix, Option<Phase>, i64, &'static str)>,
    comparison: Option<Value>,
    history: VecDeque<Value>,
    phase_seen: Option<Phase>,
}

fn rt() -> &'static Mutex<Inner> {
    static RT: OnceLock<Mutex<Inner>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(Inner::default()))
}

fn now_ms() -> i64 {
    crate::telemetry_task::get_current_timestamp_ms() as i64
}

pub(crate) fn load(dir: PathBuf) {
    let mut inner = rt().lock().unwrap();
    inner.settings = std::fs::read(dir.join(DIR).join(SETTINGS_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    inner.data_dir = Some(dir);
}

fn rocket_sender(s: &str) -> bool {
    matches!(s, "FC" | "RF")
}

/// Called for every received telemetry row (state::cache_recent_telemetry).
pub(crate) fn observe_row(row: &TelemetryRow) {
    if !rocket_sender(&row.sender_id) {
        return;
    }
    let mut inner = rt().lock().unwrap();
    inner.last_packet_ms = Some(inner.last_packet_ms.map_or(row.timestamp_ms, |t| t.max(row.timestamp_ms)));
    if row.data_type == "GPS_DATA" {
        let v = |i: usize| row.values.get(i).copied().flatten().map(f64::from).filter(|x| x.is_finite());
        if let (Some(lat), Some(lon)) = (v(0), v(1))
            && (-90.0..=90.0).contains(&lat)
            && (-180.0..=180.0).contains(&lon)
            && (lat != 0.0 || lon != 0.0)
        {
            let alt = v(2).unwrap_or(f64::NAN);
            inner.fixes.push_back(Gps { t_ms: row.timestamp_ms, lat, lon, alt, rf: row.sender_id == "RF" });
            while inner.fixes.len() > 64 || inner.fixes.front().is_some_and(|f| row.timestamp_ms - f.t_ms > 20_000) {
                inner.fixes.pop_front();
            }
        }
    }
}

/// The newest fix with a velocity from the fixes 1-4 s before it.
fn last_fix(fixes: &VecDeque<Gps>, origin: &Origin) -> Option<LastFix> {
    let last = *fixes.back()?;
    let alt = if last.alt.is_finite() { last.alt } else { fixes.iter().rev().find(|f| f.alt.is_finite())?.alt };
    let prev = fixes.iter().rev().find(|f| f.rf == last.rf && last.t_ms - f.t_ms >= 1000 && last.t_ms - f.t_ms <= 4000 && f.alt.is_finite());
    let (ve, vn, vu) = match prev {
        Some(p) => {
            let dt = (last.t_ms - p.t_ms) as f64 / 1000.0;
            let (e1, n1) = origin.en(last.lat, last.lon);
            let (e0, n0) = origin.en(p.lat, p.lon);
            (Some(round((e1 - e0) / dt, 2)), Some(round((n1 - n0) / dt, 2)), Some(round((alt - p.alt) / dt, 2)))
        }
        None => (None, None, None),
    };
    Some(LastFix {
        t_ms: last.t_ms,
        lat_deg: last.lat,
        lon_deg: last.lon,
        alt_msl_m: alt,
        ve_mps: ve,
        vn_mps: vn,
        vu_mps: vu,
        source: Some(if last.rf { "RF GPS" } else { "FC GPS" }.into()),
    })
}

fn in_flight(s: FlightState) -> bool {
    matches!(s, FlightState::Launch | FlightState::Ascent | FlightState::Coast | FlightState::Apogee | FlightState::Descent | FlightState::Reefing | FlightState::InFlight)
}

fn station_position(m: &Model) -> Option<(f64, f64)> {
    let snap = crate::antenna::snapshot();
    snap.ground_station.map(|f| (f.latitude, f.longitude)).or(Some((m.origin.lat_deg, m.origin.lon_deg)))
}

/// The flight phase now: the FC's detailed phase, else the station's state.
fn current_phase(state: &AppState) -> Option<Phase> {
    let local = state.local_flight_state_snapshot();
    let detailed = state.fc_phase.snapshot().map(|o| o.phase).filter(|p| in_flight(*p) || *p == FlightState::Landed);
    detailed.and_then(Phase::of_state).or_else(|| Phase::of_state(local))
}

/// One step of the live state machine (every second).
pub(crate) fn tick(state: &AppState) {
    let now = now_ms();
    let flying = in_flight(state.local_flight_state_snapshot()) || state.fc_phase.snapshot().is_some_and(|o| in_flight(o.phase) && o.phase != FlightState::InFlight);
    let phase = current_phase(state);
    let mut inner = rt().lock().unwrap();
    if phase.is_some() {
        inner.phase_seen = phase;
    }
    let timeout_ms = (inner.settings.timeout_s * 1000.0) as i64;
    let silent = inner.last_packet_ms.is_none_or(|t| now - t > timeout_ms);
    // Signal came back while predicting.
    if let Some((fix, ph, lost_ms)) = inner.predicting.clone()
        && !silent
    {
        inner.predicting = None;
        let by = if inner.fixes.back().is_some_and(|g| g.t_ms > fix.t_ms) { "gps_fix" } else { "signal" };
        inner.resolved = Some((fix, ph, lost_ms, by));
        inner.comparison = None;
    }
    // Compare with the first fix after the signal returned.
    if let Some((fix, ph, lost_ms, by)) = inner.resolved.clone() {
        let newer = inner.fixes.iter().find(|g| g.t_ms > fix.t_ms && g.alt.is_finite()).copied();
        if let Some(g) = newer
            && inner.comparison.is_none()
        {
            drop(inner);
            let model = Model::current(state);
            let elapsed = (g.t_ms - fix.t_ms) as f64 / 1000.0;
            let p = predict(&model, &fix, ph, elapsed, 0, 1);
            let (plat, plon) = model.origin.lat_lon(p.now_enu[0], p.now_enu[1]);
            let (err, _) = distance_bearing(plat, plon, g.lat, g.lon);
            let palt = model.origin.alt_msl_m + p.now_enu[2];
            let cmp = json!({
                "resolved_ms": now, "resolved_by": by, "lost_ms": lost_ms, "since_fix_s": round(elapsed, 1),
                "predicted": {"lat_deg": round(plat, 7), "lon_deg": round(plon, 7), "alt_msl_m": round(palt, 1)},
                "actual": {"lat_deg": g.lat, "lon_deg": g.lon, "alt_msl_m": round(g.alt, 1)},
                "error_m": round(err, 1), "alt_error_m": round(g.alt - palt, 1),
            });
            let text = format!(
                "Loss-of-signal prediction checked: signal back after {:.0} s; the predicted position was {:.0} m from the fix ({:+.0} m altitude).",
                elapsed, err, g.alt - palt
            );
            log::info!("{text}");
            crate::web::persist_message_db_only(state, text);
            inner = rt().lock().unwrap();
            if let Some(dir) = inner.data_dir.clone() {
                append_log(&dir, &cmp);
            }
            inner.history.push_front(cmp.clone());
            inner.history.truncate(HISTORY);
            inner.comparison = Some(cmp);
        } else if now - lost_ms > 30 * 60_000 && inner.comparison.is_none() && newer.is_none() && now - fix.t_ms > COMPARE_WINDOW_MS * 30 {
            inner.resolved = None;
        }
    }
    if !inner.settings.enabled || !flying {
        inner.predicting = None;
        inner.last_view = None;
        return;
    }
    if silent && inner.predicting.is_none() {
        let model = Model::current(state);
        if let Some(fix) = last_fix(&inner.fixes, &model.origin) {
            let ph = phase.or(inner.phase_seen);
            let lost = inner.last_packet_ms.unwrap_or(now);
            log::warn!(
                "{LABEL}: no rocket packet for {:.0} s in flight; predicting from the last fix ({:.6}, {:.6}, {:.0} m MSL, phase {:?}).",
                (now - lost) as f64 / 1000.0,
                fix.lat_deg,
                fix.lon_deg,
                fix.alt_msl_m,
                ph
            );
            inner.predicting = Some((fix, ph, lost));
            inner.resolved = None;
            inner.comparison = None;
        }
    }
    if let Some((fix, ph, lost)) = inner.predicting.clone() {
        drop(inner);
        let model = Model::current(state);
        let age = (now - fix.t_ms) as f64 / 1000.0;
        let p = predict(&model, &fix, ph, age, SAMPLES, (fix.t_ms as u64) ^ 0x5eed);
        let mut view = prediction_json(&model, &fix, &p, age, station_position(&model));
        view["signal_lost_ms"] = json!(lost);
        view["since_fix_s"] = json!(round(age, 1));
        view["last_fix"] = last_fix_json(&model, &fix);
        // The predicted landing area becomes the recovery search's target (an
        // operator's own area is kept).
        let area: Option<Vec<[f64; 2]>> = view["overlay"]["areas"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["kind"] == "landing_ellipse"))
            .and_then(|x| serde_json::from_value(x["polygon"].clone()).ok());
        rt().lock().unwrap().last_view = Some(view);
        if let Some(polygon) = area
            && let Err(e) = crate::recovery_search::set_target_area(state, &polygon, "los_prediction")
        {
            log::warn!("{LABEL}: the landing area was not passed to the recovery search: {e}");
        }
    }
}

fn last_fix_json(m: &Model, fix: &LastFix) -> Value {
    json!({
        "t_ms": fix.t_ms, "lat_deg": fix.lat_deg, "lon_deg": fix.lon_deg, "alt_msl_m": round(fix.alt_msl_m, 1),
        "alt_agl_m": round(fix.alt_msl_m - m.origin.alt_msl_m, 1), "ve_mps": fix.ve_mps, "vn_mps": fix.vn_mps, "vu_mps": fix.vu_mps, "source": fix.source,
    })
}

fn append_log(dir: &Path, value: &Value) {
    use std::io::Write;
    let folder = dir.join(DIR);
    if std::fs::create_dir_all(&folder).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(folder.join(LOG_FILE)) {
        let _ = writeln!(f, "{}", serde_json::to_string(value).unwrap_or_default());
    }
}

/// The route's view of the live state.
pub(crate) fn view(state: &AppState) -> Value {
    let flying = in_flight(state.local_flight_state_snapshot());
    let inner = rt().lock().unwrap();
    let st = if !inner.settings.enabled {
        "idle"
    } else if inner.predicting.is_some() {
        "predicting"
    } else if inner.resolved.is_some() {
        "resolved"
    } else if flying {
        "armed"
    } else {
        "idle"
    };
    let mut v = json!({
        "state": st, "label": LABEL, "advisory": true, "settings": inner.settings,
        "flight_state": format!("{:?}", state.local_flight_state_snapshot()),
        "last_fix": null, "signal_lost_ms": null, "since_fix_s": null, "phase": null, "predicted_now": null, "landing": null,
        "station": null, "from_station": null, "wind_source": null, "overlay": null,
        "comparison": inner.comparison, "history": inner.history,
    });
    if st == "predicting"
        && let Some(view) = &inner.last_view
        && let Some(obj) = view.as_object()
    {
        for (k, x) in obj {
            v[k] = x.clone();
        }
    }
    v
}

/// The watch's predicted target while predicting: (lat, lon, alt, age ms).
pub(crate) fn watch_target() -> Option<(f64, f64, f64, i64)> {
    let inner = rt().lock().unwrap();
    inner.predicting.as_ref()?;
    let v = inner.last_view.as_ref()?;
    let p = &v["predicted_now"];
    Some((p["lat_deg"].as_f64()?, p["lon_deg"].as_f64()?, p["alt_msl_m"].as_f64()?, (v["since_fix_s"].as_f64()? * 1000.0) as i64))
}

pub(crate) fn spawn_tick(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut shutdown = state.shutdown_subscribe();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let st = state.clone();
                    let _ = tokio::task::spawn_blocking(move || tick(&st)).await;
                }
                _ = shutdown.recv() => break,
            }
        }
    });
}

// ------------------------------------------------------------ HTTP

fn error(status: StatusCode, text: impl Into<String>) -> Response {
    (status, Json(json!({"error": text.into()}))).into_response()
}

async fn get_view(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return r;
    }
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(view(&state))).into_response()
}

async fn put_settings(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let p = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if !p.is_admin() {
        return error(StatusCode::FORBIDDEN, "Only an administrator may change the loss-of-signal prediction settings.");
    }
    let settings: Settings = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("Invalid settings: {e}")),
    };
    if !(2.0..=60.0).contains(&settings.timeout_s) {
        return error(StatusCode::BAD_REQUEST, "The timeout must be within 2..60 s.");
    }
    let dir = {
        let mut inner = rt().lock().unwrap();
        inner.settings = settings.clone();
        inner.data_dir.clone()
    };
    if let Some(dir) = dir {
        let folder = dir.join(DIR);
        let saved = std::fs::create_dir_all(&folder).and_then(|_| std::fs::write(folder.join(SETTINGS_FILE), serde_json::to_vec_pretty(&settings).unwrap_or_default()));
        if let Err(e) = saved {
            return error(StatusCode::INTERNAL_SERVER_ERROR, format!("The settings were not saved: {e}"));
        }
    }
    crate::web::persist_message_db_only(
        &state,
        format!("Loss-of-signal prediction settings changed by {}: enabled {}, timeout {:.0} s.", p.username.unwrap_or_default(), settings.enabled, settings.timeout_s),
    );
    Json(view(&state)).into_response()
}

#[derive(Deserialize)]
struct SimulateRequest {
    last_fix: LastFix,
    #[serde(default)]
    phase: Option<String>,
    elapsed_s: f64,
    #[serde(default)]
    site: Option<planner::weather::Site>,
    #[serde(default)]
    samples: Option<u32>,
}

/// A prediction for a simulated signal cut (replay): pure computation.
pub(crate) fn simulate(state: &AppState, body: &[u8]) -> Result<Value, (StatusCode, String)> {
    let req: SimulateRequest = serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("Invalid request: {e}")))?;
    let f = &req.last_fix;
    let ok = f.lat_deg.is_finite() && f.lon_deg.is_finite() && f.alt_msl_m.is_finite() && f.lat_deg.abs() <= 90.0 && f.lon_deg.abs() <= 180.0;
    if !ok || !(0.0..=3600.0).contains(&req.elapsed_s) {
        return Err((StatusCode::BAD_REQUEST, "last_fix needs a finite position and elapsed_s 0..3600.".into()));
    }
    let phase = match req.phase.as_deref() {
        None | Some("") => None,
        Some(p) => Some(Phase::parse(p).ok_or((StatusCode::BAD_REQUEST, "phase must be boost, coast, drogue or main.".to_string()))?),
    };
    let snap = planner::current_weather(state);
    let site = req.site.unwrap_or_else(|| planner::launch_site(state));
    let model = Model::from_weather(&snap, site);
    let p = predict(&model, f, phase, req.elapsed_s, req.samples.unwrap_or(SAMPLES).min(200), 1);
    let mut v = prediction_json(&model, f, &p, req.elapsed_s, station_position(&model));
    v["state"] = json!("predicting");
    v["label"] = json!(LABEL);
    v["advisory"] = json!(true);
    v["simulated"] = json!(true);
    v["since_fix_s"] = json!(req.elapsed_s);
    v["signal_lost_ms"] = json!(f.t_ms);
    v["last_fix"] = last_fix_json(&model, f);
    v["comparison"] = Value::Null;
    v["history"] = json!([]);
    Ok(v)
}

async fn post_simulate(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(r) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return r;
    }
    match tokio::task::spawn_blocking(move || simulate(&state, &body)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((s, t))) => error(s, t),
        Err(_) => error(StatusCode::INTERNAL_SERVER_ERROR, "The prediction failed."),
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/los_prediction", get(get_view))
        .route("/api/los_prediction/settings", put(put_settings))
        .route("/api/los_prediction/simulate", post(post_simulate))
}

#[cfg(test)]
#[path = "los_predict_tests.rs"]
mod tests;
