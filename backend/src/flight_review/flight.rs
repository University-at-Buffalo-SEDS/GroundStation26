//! Flight summary: apogee, deployments, landing and the flight's maxima,
//! computed from FC telemetry (live) or from a recording (post-flight).
//!
//! Inputs, in order of preference:
//! - **phases:** the FC's detailed phase (`GS_FC_PHASE` rows, the live log),
//!   the FC's FLIGHT_STATE and the station's own flight state;
//! - **deployments:** the FC's deployment messages ("PD/PR approx altitude"),
//!   the station's deployment notices (both in the message log) and the
//!   FC's Descent/Reefing edges;
//! - **altitude:** the FC Kalman altitude (ASCENT/DESCENT_STATE), else the
//!   barometer, else GPS above the pad;
//! - **velocity / acceleration:** the FC Kalman velocity, the station's
//!   vehicle speed, the raw accelerometer.
//!
//! In an integrated ground test (and HITL with the replay) the FC flies a
//! replayed flight and reports it only as `SIM_SAMPLE`: the summary is built
//! from those samples alone and marked SIMULATED; the stand's real sensors
//! never feed it.
use super::data::{Data, Row};
use crate::types::FlightState as F;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Point {
    pub t_ms: i64,
    pub value: f64,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Deployment {
    pub t_ms: i64,
    pub altitude_m: Option<f64>,
    pub altitude_source: String,
    /// What reported it: "FC message", "station notice", "FC phase".
    pub evidence: String,
    /// Integrated ground test: the FC logged the pulse instead of firing it.
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Event {
    pub name: String,
    pub t_ms: i64,
    /// Seconds from T-0 (main valve open).
    pub t_plus_s: Option<f64>,
    /// Seconds from the station's Launch acceptance.
    pub t_launch_s: Option<f64>,
    pub altitude_m: Option<f64>,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Position {
    pub lat: f64,
    pub lon: f64,
    pub alt_msl_m: Option<f64>,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlightSummary {
    /// Replayed (integrated ground test), HITL or simulator data.
    pub simulated: bool,
    /// Where the flight data came from, for the label above the section.
    pub data_label: String,
    pub launch_accepted_ms: Option<i64>,
    pub launch_source: String,
    /// T-0: the main valve opening (the T+ anchor).
    pub t0_ms: Option<i64>,
    pub t0_source: String,
    pub liftoff_ms: Option<i64>,
    pub liftoff_source: String,
    /// The apogee: the preferred altitude source's maximum.
    pub apogee: Option<Point>,
    /// Every altitude source's maximum (FC Kalman, barometer, GPS above the pad).
    pub altitude_maxima: Vec<Point>,
    /// The FC's own apogee / maximum velocity reports ("EV ... guess").
    pub fc_reported_apogee_m: Option<f64>,
    pub fc_reported_max_velocity_mps: Option<f64>,
    /// When the FC entered its Apogee phase.
    pub apogee_detected_ms: Option<i64>,
    pub time_to_apogee_s: Option<f64>,
    /// "liftoff", "T-0" or "Launch acceptance".
    pub time_to_apogee_from: String,
    pub drogue: Option<Deployment>,
    pub main: Option<Deployment>,
    pub landed_ms: Option<i64>,
    pub landed_source: String,
    pub flight_time_s: Option<f64>,
    pub max_velocity: Option<Point>,
    pub max_mach: Option<f64>,
    /// Magnitude, m/s² (raw accelerometer: includes gravity).
    pub max_acceleration: Option<Point>,
    pub max_acceleration_g: Option<f64>,
    pub descent_rate_drogue_mps: Option<f64>,
    pub descent_rate_main_mps: Option<f64>,
    pub pad: Option<Position>,
    pub landing: Option<Position>,
    pub landing_distance_m: Option<f64>,
    pub landing_bearing_deg: Option<f64>,
    pub latest_phase: Option<String>,
    pub events: Vec<Event>,
    pub notes: Vec<String>,
    /// (s from `curve_zero_ms`, m) of the preferred altitude source.
    pub altitude_curve: Vec<[f64; 2]>,
    pub altitude_source: String,
    /// (s from `curve_zero_ms`, m/s).
    pub velocity_curve: Vec<[f64; 2]>,
    pub velocity_source: String,
    pub curve_zero_ms: Option<i64>,
}

const G0: f64 = 9.806_65;
const MAX_CURVE_POINTS: usize = 1500;

type Pts = Vec<(i64, f64)>;

#[derive(Default)]
struct Gps {
    t_ms: i64,
    lat: f64,
    lon: f64,
    alt: Option<f64>,
}

#[derive(Default)]
struct Series {
    simulated: bool,
    fc_alt: Pts,
    fc_vel: Pts,
    baro_alt: Pts,
    gs_speed: Pts,
    accel: Pts,
    gps: Vec<Gps>,
}

fn push_imu(out: &mut Pts, t: i64, x: Option<f64>, y: Option<f64>, z: Option<f64>) {
    if let (Some(x), Some(y), Some(z)) = (x, y, z) {
        out.push((t, (x * x + y * y + z * z).sqrt()));
    }
}

fn push_gps(out: &mut Vec<Gps>, t: i64, lat: Option<f64>, lon: Option<f64>, alt: Option<f64>) {
    if let (Some(lat), Some(lon)) = (lat, lon)
        && (lat != 0.0 || lon != 0.0)
        && lat.abs() <= 90.0
        && lon.abs() <= 180.0
    {
        out.push(Gps { t_ms: t, lat, lon, alt });
    }
}

/// One replayed sample: its series name and values (the real type's order).
fn sim_sample(row: &Row) -> Option<(String, Vec<Option<f64>>)> {
    if row.data_type == "SIM_SAMPLE" {
        let values: Vec<Option<f32>> = row.values.iter().map(|v| v.map(|x| x as f32)).collect();
        let s = crate::igt::decode_sim_sample(&values)?;
        return Some((
            s.series.to_owned(),
            s.values.into_iter().map(|v| Some(f64::from(v)).filter(|v| v.is_finite())).collect(),
        ));
    }
    row.data_type
        .starts_with("SIM_")
        .then(|| (row.data_type.clone(), row.values.clone()))
}

fn extract(data: &Data) -> Series {
    let raw_sim = data.rows.iter().any(|r| r.data_type == "SIM_SAMPLE");
    let sim_rows: Vec<(i64, String, Vec<Option<f64>>)> = data
        .rows
        .iter()
        .filter(|r| {
            if raw_sim {
                r.data_type == "SIM_SAMPLE"
            } else {
                r.data_type.starts_with("SIM_")
            }
        })
        .filter_map(|r| sim_sample(r).map(|(name, v)| (r.t_ms, name, v)))
        .collect();
    let mut s = Series::default();
    if !sim_rows.is_empty() {
        s.simulated = true;
        for (t, name, v) in &sim_rows {
            let g = |i: usize| v.get(i).copied().flatten();
            match name.as_str() {
                "SIM_ASCENT_STATE" => {
                    if let Some(a) = g(4) {
                        s.fc_alt.push((*t, a));
                    }
                    if let Some(x) = g(5) {
                        s.fc_vel.push((*t, x));
                    }
                }
                "SIM_DESCENT_STATE" => {
                    if let Some(a) = g(2) {
                        s.fc_alt.push((*t, a));
                    }
                    if let Some(x) = g(3) {
                        s.fc_vel.push((*t, x));
                    }
                }
                "SIM_BAROMETER_DATA" => {
                    if let Some(a) = g(0) {
                        s.baro_alt.push((*t, a));
                    }
                }
                "SIM_GPS_DATA" => push_gps(&mut s.gps, *t, g(0), g(1), g(2)),
                "SIM_IMU_DATA" => push_imu(&mut s.accel, *t, g(0), g(1), g(2)),
                _ => {}
            }
        }
    } else {
        let has_accel = data.rows.iter().any(|r| r.data_type == "ACCEL_DATA");
        for r in &data.rows {
            match r.data_type.as_str() {
                "ASCENT_STATE" => {
                    if let Some(a) = r.v(4) {
                        s.fc_alt.push((r.t_ms, a));
                    }
                    if let Some(x) = r.v(5) {
                        s.fc_vel.push((r.t_ms, x));
                    }
                }
                "DESCENT_STATE" => {
                    if let Some(a) = r.v(2) {
                        s.fc_alt.push((r.t_ms, a));
                    }
                    if let Some(x) = r.v(3) {
                        s.fc_vel.push((r.t_ms, x));
                    }
                }
                "BAROMETER_DATA" => {
                    if let Some(a) = r.v(crate::telemetry_schema::baro::ALTITUDE_M) {
                        s.baro_alt.push((r.t_ms, a));
                    }
                }
                "GPS_DATA" => push_gps(&mut s.gps, r.t_ms, r.v(0), r.v(1), r.v(2)),
                "VEHICLE_SPEED" => {
                    if let Some(x) = r.v(0) {
                        s.gs_speed.push((r.t_ms, x.abs()));
                    }
                }
                "ACCEL_DATA" => push_imu(&mut s.accel, r.t_ms, r.v(0), r.v(1), r.v(2)),
                "IMU_DATA" if !has_accel => push_imu(&mut s.accel, r.t_ms, r.v(0), r.v(1), r.v(2)),
                _ => {}
            }
        }
    }
    for pts in [&mut s.fc_alt, &mut s.fc_vel, &mut s.baro_alt, &mut s.gs_speed, &mut s.accel] {
        pts.sort_by_key(|p| p.0);
    }
    s.gps.sort_by_key(|g| g.t_ms);
    s
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    let n = xs.len();
    Some(if n % 2 == 1 { xs[n / 2] } else { 0.5 * (xs[n / 2 - 1] + xs[n / 2]) })
}

fn max_point(pts: &[(i64, f64)], source: &str, from: Option<i64>, to: Option<i64>) -> Option<Point> {
    pts.iter()
        .filter(|(t, _)| from.is_none_or(|f| *t >= f) && to.is_none_or(|e| *t <= e))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(t, v)| Point {
            t_ms: *t,
            value: *v,
            source: source.into(),
        })
}

/// Linear interpolation at `t` between samples at most 5 s apart.
fn value_at(pts: &[(i64, f64)], t: i64) -> Option<f64> {
    let i = pts.partition_point(|p| p.0 < t);
    match (i.checked_sub(1).and_then(|j| pts.get(j)), pts.get(i)) {
        (_, Some(b)) if b.0 == t => Some(b.1),
        (Some(a), Some(b)) if b.0 - a.0 <= 5_000 => Some(a.1 + (b.1 - a.1) * (t - a.0) as f64 / (b.0 - a.0) as f64),
        (Some(a), _) if t - a.0 <= 1_000 => Some(a.1),
        (_, Some(b)) if b.0 - t <= 1_000 => Some(b.1),
        _ => None,
    }
}

/// Least-squares slope (units per second) over `[from, to]`; at least four
/// samples spanning two seconds.
fn slope(pts: &[(i64, f64)], from: i64, to: i64) -> Option<f64> {
    let w: Vec<(f64, f64)> = pts
        .iter()
        .filter(|(t, _)| *t >= from && *t <= to)
        .map(|(t, v)| ((*t - from) as f64 / 1000.0, *v))
        .collect();
    if w.len() < 4 || w.last()?.0 - w.first()?.0 < 2.0 {
        return None;
    }
    let n = w.len() as f64;
    let (mx, my) = (w.iter().map(|p| p.0).sum::<f64>() / n, w.iter().map(|p| p.1).sum::<f64>() / n);
    let sxx: f64 = w.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = w.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    (sxx > 0.0).then(|| sxy / sxx)
}

/// Great-circle distance (m) and initial bearing (deg) from `a` to `b`.
pub(crate) fn distance_bearing(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let r = 6_371_000.0;
    let (la1, lo1, la2, lo2) = (a.0.to_radians(), a.1.to_radians(), b.0.to_radians(), b.1.to_radians());
    let (dla, dlo) = (la2 - la1, lo2 - lo1);
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    let d = 2.0 * r * h.sqrt().min(1.0).asin();
    let y = dlo.sin() * la2.cos();
    let x = la1.cos() * la2.sin() - la1.sin() * la2.cos() * dlo.cos();
    (d, (y.atan2(x).to_degrees() + 360.0) % 360.0)
}

/// ISA speed of sound (m/s) at an altitude above mean sea level.
pub(crate) fn speed_of_sound(alt_msl_m: f64) -> f64 {
    let t = (288.15 - 0.0065 * alt_msl_m.clamp(-500.0, 11_000.0)).max(216.65);
    (1.4 * 287.053 * t).sqrt()
}

fn decimate(pts: &[(i64, f64)], zero: i64, keep: Option<i64>) -> Vec<[f64; 2]> {
    let step = pts.len().div_ceil(MAX_CURVE_POINTS).max(1);
    pts.iter()
        .enumerate()
        .filter(|(i, p)| i % step == 0 || *i + 1 == pts.len() || Some(p.0) == keep)
        .map(|(_, (t, v))| [((t - zero) as f64 / 1000.0 * 1000.0).round() / 1000.0, (v * 100.0).round() / 100.0])
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chute {
    Drogue,
    Main,
}

/// A deployment in a message: the FC's own ("PD/PR approx altitude: <m>"),
/// the station's notice ("Drogue parachute deployed at 812 m (FC), ...") or
/// the integrated ground test's ("SIMULATED flight: drogue DRY-RUN (not
/// fired) at 812 m, ..."). (chute, altitude, evidence, dry run).
pub(crate) fn parse_deployment(text: &str) -> Option<(Chute, Option<f64>, &'static str, bool)> {
    if let Some((kind, altitude)) = crate::recovery_events::parse_fc_message(text) {
        let chute = match kind {
            crate::recovery_events::Deployment::Drogue => Chute::Drogue,
            crate::recovery_events::Deployment::Main => Chute::Main,
        };
        return Some((chute, Some(altitude), "FC message", false));
    }
    let altitude = |rest: &str| -> Option<f64> {
        let after = rest.split_once(" at ")?.1;
        let number: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
        let tail = &after[number.len()..];
        tail.trim_start().starts_with('m').then(|| number.parse().ok()).flatten()
    };
    let text = text.trim();
    for (prefix, chute) in [
        ("Drogue parachute deployed", Chute::Drogue),
        ("Main parachute deployed", Chute::Main),
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return Some((chute, altitude(rest), "station notice", false));
        }
    }
    if let Some(rest) = text.strip_prefix("SIMULATED flight:") {
        let lower = rest.to_ascii_lowercase();
        let chute = if lower.contains("drogue") {
            Chute::Drogue
        } else if lower.contains("main") {
            Chute::Main
        } else {
            return None;
        };
        return Some((chute, altitude(rest), "station notice", rest.contains("DRY-RUN")));
    }
    None
}

fn first_at_or_after(list: &[(i64, u8)], from: Option<i64>, codes: &[F]) -> Option<i64> {
    list.iter()
        .filter(|(t, _)| from.is_none_or(|f| *t >= f))
        .find(|(_, c)| codes.iter().any(|s| *s as u8 == *c))
        .map(|(t, _)| *t)
}

/// The station-mode context of the data: (mode id, IGT active, provenance).
pub(crate) fn context(data: &Data) -> (Option<String>, bool, Option<String>) {
    let Some(v) = data.station_mode() else { return (None, false, None) };
    let mode = v["mode"].as_str().map(str::to_owned);
    let igt = v["igt_active"] == true || v["mode"] == "integrated_ground_test";
    (mode, igt, v["provenance"].as_str().map(str::to_owned))
}

/// Computes the summary; `None` when the data holds no flight at all (no
/// flight phase, no Launch and no altitude/velocity telemetry).
pub fn compute(data: &Data, mode_hint: Option<&str>, provenance_hint: Option<&str>) -> Option<FlightSummary> {
    let s = extract(data);
    let (ctx_mode, ctx_igt, ctx_provenance) = context(data);
    let mode = ctx_mode.as_deref().or(mode_hint);
    let provenance = ctx_provenance.as_deref().or(provenance_hint);
    // FC phases: its detailed reports and its FLIGHT_STATE; the station's own.
    let mut fc: Vec<(i64, u8)> = data
        .rows
        .iter()
        .filter(|r| r.data_type == "GS_FC_PHASE" || (r.data_type == "FLIGHT_STATE" && r.from_fc()))
        .filter_map(|r| r.v(0).filter(|v| (0.0..=255.0).contains(v)).map(|v| (r.t_ms, v as u8)))
        .collect();
    fc.sort_by_key(|p| p.0);
    let mut station: Vec<(i64, u8)> = data.station_states.clone();
    station.extend(
        data.rows
            .iter()
            .filter(|r| r.data_type == "FLIGHT_STATE" && !r.from_fc())
            .filter_map(|r| r.v(0).filter(|v| (0.0..=255.0).contains(v)).map(|v| (r.t_ms, v as u8))),
    );
    station.sort_by_key(|p| p.0);
    let deployments: Vec<(i64, Chute, Option<f64>, &'static str, bool)> = data
        .messages
        .iter()
        .filter_map(|(t, text)| parse_deployment(text).map(|(c, a, e, d)| (*t, c, a, e, d)))
        .collect();
    let mut fc_apogee = None;
    let mut fc_maxvel = None;
    for (_, text) in &data.messages {
        let text = text.trim();
        let text = text.strip_prefix("FC:").map(str::trim).unwrap_or(text);
        match crate::flight_maxima::parse_fc_message(text) {
            Some((crate::flight_maxima::Quantity::Altitude, v)) => fc_apogee = Some(v),
            Some((crate::flight_maxima::Quantity::Speed, v)) => fc_maxvel = Some(v),
            _ => {}
        }
    }
    let flying = [F::Launch, F::Ascent, F::Coast, F::Apogee, F::Descent, F::Reefing, F::InFlight];
    let has_flight = !s.fc_alt.is_empty()
        || !s.baro_alt.is_empty()
        || !s.fc_vel.is_empty()
        || fc.iter().chain(&station).any(|(_, c)| flying.iter().any(|f| *f as u8 == *c))
        || !deployments.is_empty();
    if !has_flight {
        return None;
    }
    let mut out = FlightSummary {
        simulated: s.simulated || ctx_igt || mode == Some("hitl") || provenance.is_some_and(|p| p != "hardware"),
        ..FlightSummary::default()
    };
    out.data_label = if s.simulated {
        "SIMULATED flight: the FC's replay of the ground-test dataset (SIM_SAMPLE), not the stand's sensors".into()
    } else if mode == Some("hitl") {
        "Simulated (HITL): FC telemetry from simulated sensor input".into()
    } else if provenance.is_some_and(|p| p != "hardware") {
        format!("Simulated ({}): FC telemetry", provenance.unwrap_or_default().replace('_', " "))
    } else {
        "FC telemetry".into()
    };
    // Launch acceptance: the station's Launch, else the FC's.
    let launch = first_at_or_after(&station, None, &[F::Launch])
        .map(|t| (t, "station flight state"))
        .or_else(|| first_at_or_after(&fc, None, &[F::Launch]).map(|t| (t, "FC flight state")));
    if let Some((t, source)) = launch {
        out.launch_accepted_ms = Some(t);
        out.launch_source = source.into();
    }
    // T-0: the main valve opening near Launch; else the maxima's anchor; else
    // (replayed flight) the replay's own clock.
    let t0 = data
        .rows
        .iter()
        .filter(|r| r.data_type == "VALVE_STATE" && crate::types::canonical_sender_id(r.sender.trim()) == "VB")
        .filter(|r| out.launch_accepted_ms.is_none_or(|l| r.t_ms >= l - 60_000))
        .find(|r| r.v(0) == Some(1.0))
        .map(|r| (r.t_ms, "main valve open"))
        .or_else(|| {
            data.rows
                .iter()
                .filter(|r| r.data_type == "GS_FLIGHT_MAXIMA")
                .find_map(|r| {
                    let v: serde_json::Value = serde_json::from_str(r.payload.as_deref()?).ok()?;
                    v["flight_anchor_ms"].as_i64()
                })
                .map(|t| (t, "launch clock T+ anchor"))
        })
        .or_else(|| {
            let offsets: Vec<f64> = data
                .rows
                .iter()
                .filter(|r| r.data_type == "SIM_SAMPLE")
                .filter_map(|r| r.v(1).map(|t_s| r.t_ms as f64 - t_s * 1000.0))
                .collect();
            median(offsets).map(|t| (t.round() as i64, "replay clock (SIM_SAMPLE)"))
        });
    if let Some((t, source)) = t0 {
        out.t0_ms = Some(t);
        out.t0_source = source.into();
    }
    let reference = out.launch_accepted_ms.or(out.t0_ms);
    // Preferred altitude.
    let pad_site = data
        .rows
        .iter()
        .filter(|r| r.data_type == "LAUNCH_SITE")
        .find_map(|r| Some((r.v(0)?, r.v(1)?, r.v(2))));
    let gps_pre: Vec<&Gps> = s
        .gps
        .iter()
        .filter(|g| reference.map_or(true, |r| g.t_ms <= r && g.t_ms >= r - 120_000))
        .collect();
    let gps_pre: Vec<&Gps> = if gps_pre.is_empty() || reference.is_none() {
        s.gps.iter().take(10).collect()
    } else {
        gps_pre
    };
    let pad_gps = (!gps_pre.is_empty() && reference.is_some()).then(|| {
        (
            median(gps_pre.iter().map(|g| g.lat).collect()).unwrap_or_default(),
            median(gps_pre.iter().map(|g| g.lon).collect()).unwrap_or_default(),
            median(gps_pre.iter().filter_map(|g| g.alt).collect()),
        )
    });
    out.pad = pad_gps
        .map(|(lat, lon, alt)| Position {
            lat,
            lon,
            alt_msl_m: alt,
            source: "GPS before launch".into(),
        })
        .or_else(|| {
            pad_site.map(|(lat, lon, alt)| Position {
                lat,
                lon,
                alt_msl_m: alt,
                source: "configured launch site".into(),
            })
        });
    let pad_msl = out.pad.as_ref().and_then(|p| p.alt_msl_m).or(pad_site.and_then(|p| p.2));
    let gps_agl: Pts = match pad_msl.or_else(|| median(s.gps.iter().take(10).filter_map(|g| g.alt).collect())) {
        Some(base) => s.gps.iter().filter_map(|g| g.alt.map(|a| (g.t_ms, a - base))).collect(),
        None => Vec::new(),
    };
    let sim = if s.simulated { "replayed " } else { "" };
    // The first source with a usable series (five samples), else the first
    // with any.
    let candidates: [(&Pts, String); 3] = [
        (&s.fc_alt, format!("FC Kalman altitude ({sim}estimator)")),
        (&s.baro_alt, format!("{sim}barometer altitude")),
        (&gps_agl, format!("{sim}GPS altitude above the pad")),
    ];
    let pick = candidates
        .iter()
        .position(|c| c.0.len() >= 5)
        .or_else(|| candidates.iter().position(|c| !c.0.is_empty()))
        .unwrap_or(0);
    let (alt, alt_source): (&Pts, String) = (candidates[pick].0, candidates[pick].1.clone());
    out.altitude_source = if alt.is_empty() { String::new() } else { alt_source.clone() };
    let baseline = {
        let pre: Vec<f64> = alt
            .iter()
            .filter(|(t, _)| reference.is_some_and(|r| *t <= r))
            .map(|p| p.1)
            .collect();
        median(if pre.is_empty() {
            alt.iter().take(20).map(|p| p.1).collect()
        } else {
            pre
        })
        .unwrap_or(0.0)
    };
    // Liftoff: the accelerometer above 2.5 g (held), else the altitude 10 m up.
    let accel_start = out.t0_ms.or(reference).map(|t| t - 2_000);
    let liftoff_accel = s
        .accel
        .windows(3)
        .filter(|w| accel_start.is_none_or(|a| w[0].0 >= a))
        .find(|w| w[0].1 > 2.5 * G0 && w[1].1 > 1.5 * G0 && w[2].1 > 1.5 * G0)
        .map(|w| w[0].0);
    let liftoff_alt = alt
        .iter()
        .position(|(t, v)| reference.is_none_or(|r| *t >= r - 5_000) && *v > baseline + 10.0)
        .map(|i| alt[..i].iter().rev().find(|(_, v)| *v <= baseline + 1.0).map_or(alt[i].0, |p| p.0));
    if let Some(t) = liftoff_accel {
        out.liftoff_ms = Some(t);
        out.liftoff_source = "accelerometer above 2.5 g".into();
    } else if let Some(t) = liftoff_alt {
        out.liftoff_ms = Some(t);
        out.liftoff_source = format!("{alt_source} rising 10 m");
    }
    // Apogee and every source's maximum.
    out.apogee = max_point(alt, &alt_source, None, None);
    for (pts, name) in [
        (
            &s.fc_alt,
            format!("FC Kalman altitude{}", if s.simulated { " (replayed)" } else { "" }),
        ),
        (
            &s.baro_alt,
            format!("Barometer altitude{}", if s.simulated { " (replayed)" } else { "" }),
        ),
        (
            &gps_agl,
            format!("GPS altitude above the pad{}", if s.simulated { " (replayed)" } else { "" }),
        ),
    ] {
        if let Some(p) = max_point(pts, &name, None, None) {
            out.altitude_maxima.push(p);
        }
    }
    out.fc_reported_apogee_m = fc_apogee;
    out.fc_reported_max_velocity_mps = fc_maxvel;
    out.apogee_detected_ms = first_at_or_after(&fc, reference, &[F::Apogee]);
    let apogee_ms = out.apogee.as_ref().map(|p| p.t_ms);
    let from = out
        .liftoff_ms
        .map(|t| (t, "liftoff"))
        .or(out.t0_ms.map(|t| (t, "T-0")))
        .or(out.launch_accepted_ms.map(|t| (t, "Launch acceptance")));
    if let (Some(a), Some((f, name))) = (apogee_ms, from)
        && a >= f
    {
        out.time_to_apogee_s = Some(((a - f) as f64 / 10.0).round() / 100.0);
        out.time_to_apogee_from = name.into();
    }
    // Deployments: the earliest evidence; the FC's altitude when it gave one.
    let deploy = |chute: Chute, phase: F| -> Option<Deployment> {
        let messages: Vec<&(i64, Chute, Option<f64>, &'static str, bool)> = deployments.iter().filter(|d| d.1 == chute).collect();
        let edge = first_at_or_after(&fc, reference, &[phase]);
        let first_msg = messages.iter().map(|d| d.0).min();
        let t = match (edge, first_msg) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b)?,
        };
        let reported = messages.iter().filter(|d| d.2.is_some()).min_by_key(|d| (d.3 != "FC message", d.0));
        let evidence = if edge == Some(t) {
            format!("FC phase {}", crate::mission_phase::detailed_label(phase))
        } else {
            messages.iter().find(|d| d.0 == t).map(|d| d.3.to_string()).unwrap_or_default()
        };
        let (altitude_m, altitude_source) = match reported {
            Some(d) => (
                d.2,
                if d.3 == "FC message" {
                    "FC reported".to_string()
                } else {
                    "station notice".to_string()
                },
            ),
            None => (value_at(alt, t), alt_source.clone()),
        };
        Some(Deployment {
            t_ms: t,
            altitude_m: altitude_m.map(|a| (a * 10.0).round() / 10.0),
            altitude_source,
            evidence,
            dry_run: messages.iter().any(|d| d.4),
        })
    };
    out.drogue = deploy(Chute::Drogue, F::Descent);
    out.main = deploy(Chute::Main, F::Reefing);
    // Landing: the FC's (or station's) Landed, else the altitude steady near
    // the ground for ten seconds after apogee.
    let after = apogee_ms.or(reference);
    let landed = first_at_or_after(&fc, after.or(reference), &[F::Landed, F::Recovery])
        .map(|t| (t, "FC flight state Landed".to_string()))
        .or_else(|| {
            first_at_or_after(&station, after.or(reference), &[F::Landed, F::Recovery])
                .map(|t| (t, "station flight state Landed".to_string()))
        })
        .or_else(|| {
            let a = apogee_ms?;
            let apex = out.apogee.as_ref()?.value;
            if apex - baseline < 30.0 {
                return None;
            }
            let post: Vec<&(i64, f64)> = alt.iter().filter(|(t, _)| *t > a).collect();
            post.iter().enumerate().find_map(|(i, (t, v))| {
                if *v > baseline + 30.0 {
                    return None;
                }
                let window: Vec<f64> = post[i..].iter().take_while(|(u, _)| *u <= t + 10_000).map(|p| p.1).collect();
                let span = post[i..].iter().take_while(|(u, _)| *u <= t + 10_000).last().map_or(0, |p| p.0 - t);
                let (lo, hi) = window.iter().fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
                (window.len() >= 5 && span >= 8_000 && hi - lo < 6.0).then(|| (*t, format!("{alt_source} steady near the ground")))
            })
        });
    if let Some((t, source)) = landed {
        out.landed_ms = Some(t);
        out.landed_source = source;
    }
    if let (Some(l), Some(f)) = (out.landed_ms, out.liftoff_ms.or(out.t0_ms)) {
        out.flight_time_s = (l > f).then(|| ((l - f) as f64 / 10.0).round() / 100.0);
    }
    // Maxima over the ascent.
    let lo = out.liftoff_ms.map(|t| t - 1_000).or(reference);
    let hi = apogee_ms.map(|t| t + 1_000);
    let abs = |p: &Pts| -> Pts { p.iter().map(|(t, v)| (*t, v.abs())).collect() };
    out.max_velocity = max_point(
        &abs(&s.fc_vel),
        &format!("FC Kalman velocity{}", if s.simulated { " (replayed)" } else { "" }),
        lo,
        hi,
    )
    .or_else(|| max_point(&s.gs_speed, "GS vehicle speed (baro/GPS)", lo, hi));
    if let Some(v) = &out.max_velocity {
        let msl = pad_msl.unwrap_or(0.0) + value_at(alt, v.t_ms).unwrap_or(0.0);
        out.max_mach = Some((v.value / speed_of_sound(msl) * 1000.0).round() / 1000.0);
        out.notes.push(format!(
            "Mach uses the ISA speed of sound at {:.0} m above sea level{}.",
            msl,
            if pad_msl.is_none() {
                " (pad altitude unknown: taken as sea level)"
            } else {
                ""
            }
        ));
    }
    out.max_acceleration = max_point(
        &s.accel,
        &format!("Raw accelerometer magnitude{}", if s.simulated { " (replayed)" } else { "" }),
        lo,
        hi,
    );
    out.max_acceleration_g = out.max_acceleration.as_ref().map(|p| (p.value / G0 * 100.0).round() / 100.0);
    // Descent rates.
    let end = out.landed_ms.or(alt.last().map(|p| p.0));
    if let Some(d) = &out.drogue {
        let stop = out.main.as_ref().map(|m| m.t_ms).or(end).map(|t| t - 1_000);
        out.descent_rate_drogue_mps = stop
            .and_then(|stop| slope(alt, d.t_ms + 2_000, stop))
            .filter(|r| *r < 0.0)
            .map(|r| (-r * 10.0).round() / 10.0);
    }
    if let Some(m) = &out.main {
        out.descent_rate_main_mps = end
            .and_then(|e| slope(alt, m.t_ms + 3_000, e - 1_000))
            .filter(|r| *r < 0.0)
            .map(|r| (-r * 10.0).round() / 10.0);
    }
    // Landing position.
    if let Some(a) = apogee_ms.or(out.liftoff_ms) {
        let late: Vec<&Gps> = s.gps.iter().filter(|g| g.t_ms > out.landed_ms.unwrap_or(a)).collect();
        let late: Vec<&Gps> = if late.is_empty() {
            s.gps.iter().filter(|g| g.t_ms > a).collect()
        } else {
            late
        };
        let tail: Vec<&&Gps> = late.iter().rev().take(10).collect();
        if !tail.is_empty() {
            out.landing = Some(Position {
                lat: median(tail.iter().map(|g| g.lat).collect()).unwrap_or_default(),
                lon: median(tail.iter().map(|g| g.lon).collect()).unwrap_or_default(),
                alt_msl_m: median(tail.iter().filter_map(|g| g.alt).collect()),
                source: format!("{}GPS, last {} fixes", if s.simulated { "replayed " } else { "" }, tail.len()),
            });
        }
    }
    if let (Some(p), Some(l)) = (&out.pad, &out.landing) {
        let (d, b) = distance_bearing((p.lat, p.lon), (l.lat, l.lon));
        out.landing_distance_m = Some(d.round());
        out.landing_bearing_deg = Some(b.round());
    }
    out.latest_phase = fc
        .last()
        .and_then(|(_, c)| crate::types::u8_to_flight_state(*c))
        .map(|p| crate::mission_phase::detailed_label(p).to_owned());
    // Events.
    let t_rel = |t: i64, zero: Option<i64>| zero.map(|z| ((t - z) as f64 / 10.0).round() / 100.0);
    let mut events: Vec<(String, i64, Option<f64>, String)> = Vec::new();
    if let Some(t) = out.launch_accepted_ms {
        events.push(("Launch accepted".into(), t, None, out.launch_source.clone()));
    }
    if let Some(t) = out.t0_ms {
        events.push(("T-0".into(), t, None, out.t0_source.clone()));
    }
    if let Some(t) = out.liftoff_ms {
        events.push(("Liftoff".into(), t, value_at(alt, t), out.liftoff_source.clone()));
    }
    if let Some(v) = &out.max_velocity {
        events.push((
            format!("Max velocity {:.0} m/s", v.value),
            v.t_ms,
            value_at(alt, v.t_ms),
            v.source.clone(),
        ));
    }
    if let Some(p) = &out.apogee {
        events.push(("Apogee (max altitude)".into(), p.t_ms, Some(p.value), p.source.clone()));
    }
    if let Some(t) = out.apogee_detected_ms {
        events.push(("FC apogee detected".into(), t, value_at(alt, t), "FC phase Apogee".into()));
    }
    for (name, d) in [("Drogue deploy", &out.drogue), ("Main/reefing deploy", &out.main)] {
        if let Some(d) = d {
            let label = if d.dry_run {
                format!("{name} (DRY-RUN, not fired)")
            } else {
                name.to_string()
            };
            events.push((label, d.t_ms, d.altitude_m, d.evidence.clone()));
        }
    }
    if let Some(t) = out.landed_ms {
        events.push(("Touchdown / Landed".into(), t, value_at(alt, t), out.landed_source.clone()));
    }
    events.sort_by_key(|e| e.1);
    out.events = events
        .into_iter()
        .map(|(name, t, altitude, source)| Event {
            name,
            t_ms: t,
            t_plus_s: t_rel(t, out.t0_ms),
            t_launch_s: t_rel(t, out.launch_accepted_ms),
            altitude_m: altitude.map(|a| (a * 10.0).round() / 10.0),
            source,
        })
        .collect();
    // Curves for the plots.
    let zero = out.t0_ms.or(out.liftoff_ms).or(out.launch_accepted_ms).or(alt.first().map(|p| p.0));
    if let Some(z) = zero {
        out.curve_zero_ms = Some(z);
        out.altitude_curve = decimate(alt, z, apogee_ms);
        let (vel, vel_source) = if !s.fc_vel.is_empty() {
            (
                &s.fc_vel,
                format!("FC Kalman velocity{}", if s.simulated { " (replayed)" } else { "" }),
            )
        } else {
            (&s.gs_speed, "GS vehicle speed (baro/GPS)".to_string())
        };
        out.velocity_curve = decimate(vel, z, None);
        out.velocity_source = if vel.is_empty() { String::new() } else { vel_source };
    }
    if s.simulated {
        out.notes.push("Replayed flight: every value here comes from the FC's SIM_SAMPLE reports of the ground-test dataset. The stand's real barometer, GPS and accelerometer are not used.".into());
    }
    if gps_agl.is_empty() && !s.gps.is_empty() {
        out.notes.push("GPS altitude above the pad is unavailable: no pad altitude.".into());
    }
    Some(out)
}

/// The full-resolution series a flight export writes (network ms).
pub(crate) struct ExportSeries {
    pub simulated: bool,
    /// (source name, samples): FC Kalman, barometer.
    pub altitude: Vec<(String, Vec<(i64, f64)>)>,
    pub velocity: Vec<(String, Vec<(i64, f64)>)>,
    pub acceleration: Vec<(i64, f64)>,
    /// (t, lat, lon, alt MSL).
    pub gps: Vec<(i64, f64, f64, Option<f64>)>,
}

pub(crate) fn export_series(data: &Data) -> ExportSeries {
    let s = extract(data);
    let tag = if s.simulated { " (replayed)" } else { "" };
    ExportSeries {
        simulated: s.simulated,
        altitude: vec![
            (format!("FC Kalman altitude{tag}"), s.fc_alt),
            (format!("Barometer altitude{tag}"), s.baro_alt),
        ],
        velocity: vec![
            (format!("FC Kalman velocity{tag}"), s.fc_vel),
            ("GS vehicle speed".into(), s.gs_speed),
        ],
        acceleration: s.accel,
        gps: s.gps.into_iter().map(|g| (g.t_ms, g.lat, g.lon, g.alt)).collect(),
    }
}
