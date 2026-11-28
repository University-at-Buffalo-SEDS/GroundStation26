//! Flight-phase segmentation of a track (recorded and live).
//!
//! The station's flight states and the FC's detailed phase (its FLIGHT_STATE
//! and the station's `GS_FC_PHASE` rows) are mapped onto the shared phase
//! vocabulary (flight-predict CONTRACT.md): `pad`, `boost`, `coast`,
//! `drogue`, `main`, `landed`, with `apogee` as an event. The UI colours a
//! track from these segments and never derives phases itself.
//!
//! - `GET /api/recordings/{id}/replay/trajectory` carries `phases`, `events`
//!   and a `phase` on every point (recording_replay);
//! - `GET /api/flight_phases` is the live equivalent: the current flight's
//!   phases, events and rocket GPS track from the station's recent cache.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Pad,
    Boost,
    Coast,
    Drogue,
    Main,
    Landed,
}

impl Phase {
    fn in_flight(self) -> bool {
        matches!(self, Self::Boost | Self::Coast | Self::Drogue | Self::Main)
    }
}

/// `[start_ms, end_ms)`; `end_ms` null: still in that phase (open-ended).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PhaseSegment {
    pub phase: Phase,
    pub start_ms: i64,
    pub end_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PhaseEvent {
    /// `liftoff`, `burnout`, `apogee`, `drogue`, `main` or `landing`.
    pub kind: &'static str,
    pub t_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub altitude_m: Option<f64>,
}

impl PhaseEvent {
    fn at(kind: &'static str, t_ms: i64) -> Self {
        Self { kind, t_ms, latitude: None, longitude: None, altitude_m: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Segmentation {
    pub phases: Vec<PhaseSegment>,
    pub events: Vec<PhaseEvent>,
}

/// What one FlightState code means for the phase (CONTRACT.md mapping).
enum Step {
    To(Phase),
    /// Apogee: coast, plus the apogee marker.
    Apogee,
    /// InFlight with no detailed phase: boost until Coast is seen.
    InFlight,
    /// Aborted or unknown: keep the previous phase.
    Keep,
}

fn step(code: u8) -> Step {
    match code {
        0..=6 => Step::To(Phase::Pad),
        7 | 8 => Step::To(Phase::Boost),
        9 => Step::To(Phase::Coast),
        10 => Step::Apogee,
        11 => Step::To(Phase::Drogue),
        12 => Step::To(Phase::Main),
        13 | 14 => Step::To(Phase::Landed),
        16 => Step::InFlight,
        _ => Step::Keep,
    }
}

/// Segments the time-ordered FlightState reports `(t_ms, code)` (the station's
/// and the FC's together). `end_ms` closes the last segment (None: open).
pub fn segment(reports: &[(i64, u8)], end_ms: Option<i64>) -> Segmentation {
    let mut reports = reports.to_vec();
    reports.sort_by_key(|r| r.0);
    let mut out = Segmentation::default();
    let mut current: Option<Phase> = None;
    let mut apogee_seen = false;
    for (t, code) in reports {
        let next = match step(code) {
            Step::To(phase) => phase,
            Step::Apogee => {
                if !apogee_seen {
                    out.events.push(PhaseEvent::at("apogee", t));
                    apogee_seen = true;
                }
                Phase::Coast
            }
            Step::InFlight => match current {
                Some(phase) if phase.in_flight() || phase == Phase::Landed => phase,
                _ => Phase::Boost,
            },
            Step::Keep => match current {
                Some(phase) => phase,
                None => continue,
            },
        };
        if current == Some(next) {
            continue;
        }
        if next == Phase::Pad {
            apogee_seen = false;
        }
        let event = match (current, next) {
            (_, Phase::Boost) if current != Some(Phase::Coast) => Some("liftoff"),
            (Some(Phase::Boost), Phase::Coast) => Some("burnout"),
            (_, Phase::Drogue) => Some("drogue"),
            (_, Phase::Main) => Some("main"),
            (Some(p), Phase::Landed) if p.in_flight() => Some("landing"),
            _ => None,
        };
        // A descent with no Apogee report: the apogee is at the drogue edge.
        if matches!(next, Phase::Drogue | Phase::Main) && !apogee_seen && current.is_some_and(|p| matches!(p, Phase::Boost | Phase::Coast)) {
            out.events.push(PhaseEvent::at("apogee", t));
            apogee_seen = true;
        }
        if let Some(kind) = event {
            out.events.push(PhaseEvent::at(kind, t));
        }
        if let Some(last) = out.phases.last_mut() {
            last.end_ms = Some(t);
        }
        out.phases.push(PhaseSegment { phase: next, start_ms: t, end_ms: None });
        current = Some(next);
    }
    if let (Some(last), Some(end)) = (out.phases.last_mut(), end_ms) {
        last.end_ms = Some(end.max(last.start_ms));
    }
    out.events.sort_by_key(|e| e.t_ms);
    out
}

/// The phase at `t_ms`: the segment with `start_ms <= t < end_ms` (a boundary
/// belongs to the phase that starts there; the last segment also holds its
/// own end). None before the first segment.
pub fn phase_at(phases: &[PhaseSegment], t_ms: i64) -> Option<Phase> {
    let i = phases.partition_point(|s| s.start_ms <= t_ms);
    let segment = phases.get(i.checked_sub(1)?)?;
    let last = i == phases.len();
    match segment.end_ms {
        Some(end) if t_ms > end || (t_ms == end && !last) => None,
        _ => Some(segment.phase),
    }
}

/// Positions the events on a time-ordered track `(t_ms, lat, lon, alt)` by
/// linear interpolation (events outside the track keep no position).
pub fn place_events(events: &mut [PhaseEvent], track: &[(i64, f64, f64, f64)]) {
    for event in events.iter_mut() {
        let i = track.partition_point(|p| p.0 < event.t_ms);
        let point = match (i.checked_sub(1).and_then(|j| track.get(j)), track.get(i)) {
            (_, Some(b)) if b.0 == event.t_ms => Some((b.1, b.2, b.3)),
            (Some(a), Some(b)) => {
                let f = (event.t_ms - a.0) as f64 / (b.0 - a.0).max(1) as f64;
                Some((a.1 + (b.1 - a.1) * f, a.2 + (b.2 - a.2) * f, a.3 + (b.3 - a.3) * f))
            }
            _ => None,
        };
        if let Some((lat, lon, alt)) = point {
            event.latitude = Some(lat);
            event.longitude = Some(lon);
            event.altitude_m = Some((alt * 10.0).round() / 10.0);
        }
    }
}

// ------------------------------------------------------------------ live

const MAX_LIVE_POINTS: usize = 2_000;

#[derive(Debug, Serialize)]
pub struct LiveTrackPoint {
    pub timestamp_ms: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: f64,
    pub phase: Option<Phase>,
}

#[derive(Debug, Serialize)]
pub struct LiveFlightPhases {
    pub source: &'static str,
    /// Start of the flight window read (the latest launch capture), if any.
    pub window_start_ms: Option<i64>,
    pub current_phase: Option<Phase>,
    pub phases: Vec<PhaseSegment>,
    pub events: Vec<PhaseEvent>,
    /// The rocket GPS sender the track uses (RF preferred, then FC).
    pub source_sender_id: Option<String>,
    pub points: Vec<LiveTrackPoint>,
    pub sampled: bool,
}

/// The live flight from the station's recent cache and its live phase log.
pub(crate) fn live(state: &AppState) -> LiveFlightPhases {
    use crate::flight_review::data;
    let window = state
        .recording_store
        .captures()
        .into_iter()
        .filter(|c| matches!(c.kind, crate::recording_store::EventKind::Launch | crate::recording_store::EventKind::Hitl))
        .max_by_key(|c| c.event_start_ms)
        .map(|c| c.window_start_ms);
    let from = window.unwrap_or(i64::MIN);
    let (mut reports, radio, fc) = from_cache(state, from);
    let mut log = data::Data::default();
    data::add_live_log(&mut log, from);
    reports.extend(log.of_type("GS_FC_PHASE").filter_map(|r| r.v(0).filter(|v| (0.0..=255.0).contains(v)).map(|v| (r.t_ms, v as u8))));
    reports.extend(log.station_states.iter().copied());
    assemble(window, &reports, radio, fc)
}

type Fix = (i64, f64, f64, f64);

/// FLIGHT_STATE reports and the RF and FC rocket fixes in the recent cache from `from`.
fn from_cache(state: &AppState, from: i64) -> (Vec<(i64, u8)>, Vec<Fix>, Vec<Fix>) {
    let mut reports: Vec<(i64, u8)> = Vec::new();
    let (mut radio, mut fc) = (Vec::new(), Vec::new());
    let cache = state.recent_telemetry_cache.lock().unwrap();
    for row in cache.iter().filter(|r| r.timestamp_ms >= from) {
        let sender = crate::types::canonical_sender_id(row.sender_id.trim());
        match crate::telemetry_schema::canonical_type_name(&row.data_type) {
            "FLIGHT_STATE" => {
                if let Some(Some(v)) = row.values.first()
                    && (0.0..=255.0).contains(v)
                {
                    reports.push((row.timestamp_ms, *v as u8));
                }
            }
            "GPS_DATA" | "GPS" | "ROCKET_GPS" if sender == "RF" || sender == "FC" => {
                let value = |i: usize| row.values.get(i).copied().flatten().map(f64::from).filter(|v| v.is_finite());
                if let (Some(lat), Some(lon), Some(alt)) = (value(0), value(1), value(2))
                    && lat.abs() <= 90.0
                    && lon.abs() <= 180.0
                    && (lat != 0.0 || lon != 0.0)
                {
                    if sender == "RF" { &mut radio } else { &mut fc }.push((row.timestamp_ms, lat, lon, alt));
                }
            }
            _ => {}
        }
    }
    (reports, radio, fc)
}

/// The live document from the flight's reports and its RF and FC GPS fixes.
pub(crate) fn assemble(window: Option<i64>, reports: &[(i64, u8)], radio: Vec<Fix>, fc: Vec<Fix>) -> LiveFlightPhases {
    let (mut track, sender) = if !radio.is_empty() { (radio, Some("RF")) } else if !fc.is_empty() { (fc, Some("FC")) } else { (Vec::new(), None) };
    track.sort_by_key(|p| p.0);
    let mut seg = segment(reports, None);
    place_events(&mut seg.events, &track);
    let sampled = track.len() > MAX_LIVE_POINTS;
    if sampled {
        let n = track.len();
        let mut kept: Vec<_> = (0..MAX_LIVE_POINTS - 1).map(|i| track[i * (n - 1) / (MAX_LIVE_POINTS - 1)]).collect();
        kept.push(track[n - 1]);
        track = kept;
    }
    LiveFlightPhases {
        source: "live",
        window_start_ms: window,
        current_phase: seg.phases.last().map(|s| s.phase),
        points: track
            .iter()
            .map(|&(t, lat, lon, alt)| LiveTrackPoint { timestamp_ms: t, latitude: lat, longitude: lon, altitude_m: alt, phase: phase_at(&seg.phases, t) })
            .collect(),
        phases: seg.phases,
        events: seg.events,
        source_sender_id: sender.map(str::to_owned),
        sampled,
    }
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/flight_phases", get(get_live))
}

async fn get_live(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    let body = live(&state);
    ([(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Phase::*;

    fn kinds(s: &Segmentation) -> Vec<(&str, i64)> {
        s.events.iter().map(|e| (e.kind, e.t_ms)).collect()
    }

    #[test]
    fn nominal_flight_maps_every_state() {
        // Armed, Launch, Ascent, Coast, Apogee, Descent, Reefing, Landed, Recovery.
        let s = segment(&[(0, 6), (10, 7), (12, 8), (20, 9), (40, 10), (41, 11), (90, 12), (200, 13), (210, 14)], Some(300));
        let phases: Vec<_> = s.phases.iter().map(|p| (p.phase, p.start_ms, p.end_ms)).collect();
        assert_eq!(phases, vec![
            (Pad, 0, Some(10)),
            (Boost, 10, Some(20)),
            (Coast, 20, Some(41)),
            (Drogue, 41, Some(90)),
            (Main, 90, Some(200)),
            (Landed, 200, Some(300)),
        ]);
        assert_eq!(kinds(&s), vec![("liftoff", 10), ("burnout", 20), ("apogee", 40), ("drogue", 41), ("main", 90), ("landing", 200)]);
    }

    #[test]
    fn every_point_gets_its_phase_including_at_the_boundaries() {
        let s = segment(&[(0, 6), (10, 7), (20, 9), (40, 11), (90, 12), (200, 13)], Some(300));
        let expect = [
            (-1, None),
            (0, Some(Pad)),
            (9, Some(Pad)),
            (10, Some(Boost)),
            (19, Some(Boost)),
            (20, Some(Coast)),
            (39, Some(Coast)),
            (40, Some(Drogue)),
            (89, Some(Drogue)),
            (90, Some(Main)),
            (199, Some(Main)),
            (200, Some(Landed)),
            (300, Some(Landed)),
            (301, None),
        ];
        for (t, phase) in expect {
            assert_eq!(phase_at(&s.phases, t), phase, "t={t}");
        }
        // Open-ended (live): the last phase holds onward.
        let live = segment(&[(0, 6), (10, 16)], None);
        assert_eq!(phase_at(&live.phases, 1_000_000), Some(Boost));
        assert_eq!(phase_at(&[], 5), None);
    }

    #[test]
    fn in_flight_is_boost_until_coast_and_never_hides_a_detailed_phase() {
        // Network InFlight only (station), then the FC's detailed reports.
        let s = segment(&[(0, 6), (10, 7), (11, 16), (20, 9), (25, 16), (40, 11), (45, 16), (90, 13), (95, 16)], None);
        let phases: Vec<_> = s.phases.iter().map(|p| p.phase).collect();
        assert_eq!(phases, vec![Pad, Boost, Coast, Drogue, Landed]);
        // A descent with no Apogee report puts the apogee at the drogue edge.
        assert_eq!(kinds(&s), vec![("liftoff", 10), ("burnout", 20), ("apogee", 40), ("drogue", 40), ("landing", 90)]);
        // InFlight from the pad (no Launch seen) is boost.
        let s = segment(&[(0, 6), (5, 16)], None);
        assert_eq!(s.phases[1].phase, Boost);
        assert_eq!(kinds(&s), vec![("liftoff", 5)]);
    }

    #[test]
    fn aborted_keeps_the_previous_phase_and_reports_are_sorted() {
        let s = segment(&[(30, 15), (0, 6), (20, 7)], Some(40));
        assert_eq!(s.phases.iter().map(|p| p.phase).collect::<Vec<_>>(), vec![Pad, Boost]);
        assert_eq!(s.phases[1].end_ms, Some(40));
        // Aborted before anything is known adds nothing.
        assert!(segment(&[(0, 15)], None).phases.is_empty());
        assert!(segment(&[], Some(5)).phases.is_empty());
    }

    #[test]
    fn events_are_placed_on_the_track() {
        let mut s = segment(&[(0, 6), (1000, 7), (3000, 9), (5000, 10)], None);
        let track = [(0, 31.0, -102.0, 900.0), (2000, 31.0, -102.0, 1100.0), (4000, 31.002, -102.0, 1500.0), (6000, 31.002, -102.0, 1600.0)];
        place_events(&mut s.events, &track);
        let burnout = s.events.iter().find(|e| e.kind == "burnout").unwrap();
        assert_eq!(burnout.altitude_m, Some(1300.0));
        assert!((burnout.latitude.unwrap() - 31.001).abs() < 1e-9);
        let liftoff = &s.events[0];
        assert_eq!(liftoff.altitude_m, Some(1000.0));
        let mut late = vec![PhaseEvent::at("landing", 9000)];
        place_events(&mut late, &track);
        assert!(late[0].latitude.is_none());
    }

    #[tokio::test]
    async fn live_document_reads_the_cache() {
        let state = crate::state::tests::test_app_state().await;
        {
            let mut cache = state.recent_telemetry_cache.lock().unwrap();
            for (t, ty, sender, values) in [
                (1_000i64, "FLIGHT_STATE", "FC", vec![Some(6.0f32)]),
                (2_000, "FLIGHT_STATE", "FC", vec![Some(7.0)]),
                (2_500, "GPS_DATA", "FC", vec![Some(31.758), Some(-102.046), Some(900.0)]),
                (3_000, "FLIGHT_STATE", "FC", vec![Some(9.0)]),
                (3_500, "GPS_DATA", "FC", vec![Some(31.759), Some(-102.046), Some(1500.0)]),
            ] {
                cache.push_back(crate::types::TelemetryRow {
                    timestamp_ms: t,
                    data_type: ty.into(),
                    sender_id: sender.into(),
                    values,
                    over_range: 0,
                    over_range_low: 0,
                });
            }
        }
        // The process-wide live log is shared with other tests: read the cache part alone.
        let (reports, radio, fc) = from_cache(&state, i64::MIN);
        assert_eq!(reports, vec![(1_000, 6), (2_000, 7), (3_000, 9)]);
        assert!(radio.is_empty());
        let live = assemble(None, &reports, radio, fc);
        assert_eq!(live.source_sender_id.as_deref(), Some("FC"));
        assert_eq!(live.points.len(), 2);
        assert_eq!(live.points[0].phase, Some(Boost));
        assert_eq!(live.points[1].phase, Some(Coast));
        assert_eq!(live.current_phase, Some(Coast));
        let json = serde_json::to_value(&live).unwrap();
        assert_eq!(json["phases"][0]["phase"], "pad");
        assert_eq!(json["events"][1]["kind"], "burnout");
    }
}
