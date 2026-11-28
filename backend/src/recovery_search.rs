//! Recovery search coverage (docs/backend/recovery-search.md).
//!
//! After landing, an operator starts a recovery search session. Searchers
//! who explicitly opt in on their phone or watch share their GPS track while
//! the session is on; the station unions the tracks, each buffered by the
//! swath width (how far either side a searcher can see the rocket), and
//! reports how much of the predicted landing area has been searched and which
//! parts have not. The result is a map overlay (`search_coverage`) plus stats.
//!
//! Privacy (the proximity lock keeps location on the device; this is the one
//! feature that sends a track, so it is opt-in and narrow):
//! - A device shares only after its user opts in for the current session.
//!   The opt-in is recorded here per user and device and returns an opt-in
//!   id that every upload must carry. Opting out (any time) voids the id.
//! - Uploads are refused when no session is on (apart from a short grace for
//!   fixes recorded before the stop that were buffered out of signal), for a
//!   different session, or without a valid opt-in. Fixes from before the
//!   opt-in are refused.
//! - Tracks live only for the session: starting a new session drops them, an
//!   administrator can clear them, a searcher can delete their own. They are
//!   kept in memory and in `recovery_search.json` in the data directory (so a
//!   station restart mid-search does not lose them), never in the telemetry
//!   database; station messages name devices, never places.
//!
//! Uploads are idempotent: each fix carries the device's sequence number, and
//! the station keeps one fix per (user, device, seq), so a retried or late
//! batch merges in place and coverage orders fixes by time.
use crate::auth::{AuthPrincipal, Permission};
use crate::state::AppState;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) mod coverage;
#[cfg(test)]
mod tests;

pub const FORMAT: &str = "gs26.recovery_search";
const FILE: &str = "recovery_search.json";
pub const SWATH_RANGE_M: std::ops::RangeInclusive<f64> = 2.0..=500.0;
pub const DEFAULT_SWATH_M: f64 = 30.0;
pub const MAX_BATCH_POINTS: usize = 500;
pub const MAX_POINTS_PER_DEVICE: usize = 20_000;
pub const MAX_POINTS_TOTAL: usize = 200_000;
pub const MAX_DEVICES: usize = 64;
const MAX_DEVICES_PER_USER: usize = 8;
/// After a stop, fixes recorded before the stop are still accepted this long
/// (a searcher out of signal uploads when back in range).
pub const LATE_UPLOAD_GRACE_S: i64 = 1800;
/// Device clock tolerance for the time checks.
const CLOCK_SKEW_MS: i64 = 120_000;
/// Worst accepted fix accuracy.
pub const MAX_ACCURACY_M: f64 = 100.0;
pub const POLYGON_MAX_POINTS: usize = 200;
/// Largest accepted target extent (bounding-box diagonal).
const POLYGON_MAX_SPAN_M: f64 = 30_000.0;
/// A replay bundle includes search fixes up to this long after the recording's last row.
pub const BUNDLE_AFTER_END_MS: i64 = 6 * 3600 * 1000;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RATE_MAX: usize = 30;
/// Most displayed points per searcher track (the coverage uses all of them).
const TRACK_DISPLAY_MAX: usize = 2_000;
const NAME_MAX: usize = 48;
/// Per-searcher track colours (CONTRACT.md palette).
pub const COLORS: [&str; 6] = ["#0ea5e9", "#eab308", "#ec4899", "#14b8a6", "#f97316", "#8b5cf6"];
/// Target sources an automatic caller may set. An operator's manual target
/// is never replaced automatically.
pub const SOURCE_MANUAL: &str = "manual";

// ---------------------------------------------------------------------------
// State

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub swath_width_m: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { swath_width_m: DEFAULT_SWATH_M }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    /// `[lat, lon]` closed ring, first point not repeated.
    pub polygon: Vec<[f64; 2]>,
    /// `los_prediction`, `flight_plan`, `manual`, ...
    pub source: String,
    pub label: Option<String>,
    pub set_ms: i64,
    pub set_by: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Point {
    t_ms: i64,
    lat: f64,
    lon: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acc_m: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Searcher {
    username: String,
    device_id: String,
    name: String,
    platform: String,
    color: usize,
    sharing: bool,
    #[serde(default)]
    opt_in_id: Option<String>,
    opted_in_ms: Option<i64>,
    opted_out_ms: Option<i64>,
    /// One fix per device sequence number.
    points: BTreeMap<u64, Point>,
    last_upload_ms: Option<i64>,
    #[serde(skip)]
    recent: VecDeque<Instant>,
}

impl Searcher {
    /// Fixes in time order (ties by sequence).
    fn ordered(&self) -> Vec<Point> {
        let mut list: Vec<(u64, Point)> = self.points.iter().map(|(s, p)| (*s, *p)).collect();
        list.sort_by(|a, b| a.1.t_ms.cmp(&b.1.t_ms).then(a.0.cmp(&b.0)));
        list.into_iter().map(|(_, p)| p).collect()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Session {
    session_id: Option<String>,
    active: bool,
    started_ms: Option<i64>,
    started_by: Option<String>,
    stopped_ms: Option<i64>,
    stopped_by: Option<String>,
    settings: Settings,
    target: Option<Target>,
    searchers: BTreeMap<String, Searcher>,
    cleared_ms: Option<i64>,
    cleared_by: Option<String>,
}

struct Cached {
    revision: u64,
    body: Value,
}

#[derive(Default)]
struct Inner {
    data_dir: Option<PathBuf>,
    session: Session,
    revision: u64,
    saved_revision: u64,
}

#[derive(Default)]
pub struct Store {
    inner: Mutex<Inner>,
    cache: Mutex<Option<Cached>>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn key_of(username: &str, device_id: &str) -> String {
    format!("{}:{device_id}", username.to_lowercase())
}

/// A stable public id for a searcher (never the raw device id).
fn public_id(key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    digest.as_ref()[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(bytes: usize) -> String {
    (0..bytes).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}

fn valid_device_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn clean_text(text: &str, max: usize) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && text.chars().count() <= max && !text.chars().any(char::is_control)).then(|| text.to_owned())
}

fn signed_in(principal: &AuthPrincipal) -> Option<String> {
    principal.username.clone().filter(|u| !principal.anonymous && !u.is_empty())
}

type Refusal = (StatusCode, &'static str, String);

fn refusal(status: StatusCode, code: &'static str, text: impl Into<String>) -> Refusal {
    (status, code, text.into())
}

fn distance_m(points: &[Point]) -> f64 {
    let Some(first) = points.first() else { return 0.0 };
    let frame = coverage::Frame::new(first.lat, first.lon);
    points
        .windows(2)
        .map(|w| {
            let (a, b) = (frame.to_local(w[0].lat, w[0].lon), frame.to_local(w[1].lat, w[1].lon));
            let d = (b.0 - a.0).hypot(b.1 - a.1);
            // A jump across a gap is not distance walked.
            if d > coverage::GAP_M || w[1].t_ms - w[0].t_ms > coverage::GAP_MS { 0.0 } else { d }
        })
        .sum()
}

impl Store {
    fn bump(inner: &mut Inner) {
        inner.revision += 1;
    }

    /// Late uploads are accepted until this time after a stop.
    fn accepting(session: &Session, now_ms: i64) -> bool {
        session.session_id.is_some()
            && (session.active || session.stopped_ms.is_some_and(|s| now_ms <= s + LATE_UPLOAD_GRACE_S * 1000))
    }

    /// The status document (without the per-caller `you`), cached per revision.
    pub(crate) fn status_body(&self, now_ms: i64) -> Value {
        let (revision, session) = {
            let inner = self.inner.lock().unwrap();
            if let Some(cached) = self.cache.lock().unwrap().as_ref()
                && cached.revision == inner.revision
            {
                let mut body = cached.body.clone();
                body["accepting_uploads"] = Self::accepting(&inner.session, now_ms).into();
                return body;
            }
            (inner.revision, inner.session.clone())
        };
        let body = build_status(&session, now_ms);
        let mut cache = self.cache.lock().unwrap();
        if cache.as_ref().is_none_or(|c| c.revision <= revision) {
            *cache = Some(Cached { revision, body: body.clone() });
        }
        body
    }

}

fn build_status(session: &Session, now_ms: i64) -> Value {
    let searchers: Vec<(&String, &Searcher, Vec<Point>)> =
        session.searchers.iter().map(|(k, s)| (k, s, s.ordered())).collect();
    let tracks: Vec<Vec<coverage::Fix>> = searchers
        .iter()
        .map(|(_, _, pts)| pts.iter().map(|p| coverage::Fix { lat: p.lat, lon: p.lon, t_ms: p.t_ms }).collect())
        .collect();
    let target = session.target.as_ref().map(|t| t.polygon.as_slice());
    let cov = coverage::compute(&tracks, target, session.settings.swath_width_m);
    let session_tag = session.session_id.clone().unwrap_or_else(|| "none".into());
    let mut overlay_tracks = Vec::new();
    let mut list = Vec::new();
    let (mut points_total, mut distance_total) = (0usize, 0.0f64);
    for (key, s, pts) in &searchers {
        let id = public_id(key);
        let color = COLORS[s.color % COLORS.len()];
        let distance = distance_m(pts);
        points_total += pts.len();
        distance_total += distance;
        list.push(json!({
            "id": id, "name": s.name, "username": s.username, "platform": s.platform, "color": color,
            "sharing": s.sharing && session.active, "points": pts.len(), "distance_m": (distance * 10.0).round() / 10.0,
            "first_ms": pts.first().map(|p| p.t_ms), "last_ms": pts.last().map(|p| p.t_ms),
            "opted_in_ms": s.opted_in_ms, "opted_out_ms": s.opted_out_ms,
        }));
        if pts.is_empty() {
            continue;
        }
        let stride = pts.len().div_ceil(TRACK_DISPLAY_MAX).max(1);
        let mut shown: Vec<Value> =
            pts.iter().step_by(stride).map(|p| json!([p.lat, p.lon, Value::Null])).collect();
        if (pts.len() - 1) % stride != 0 {
            let last = pts.last().unwrap();
            shown.push(json!([last.lat, last.lon, Value::Null]));
        }
        overlay_tracks.push(json!({
            "id": format!("searcher:{id}"), "phase": Value::Null, "label": s.name, "style": "search", "color": color,
            "points": shown,
        }));
    }
    let mut areas = Vec::new();
    for (i, polygon) in cov.swath_polygons.iter().enumerate() {
        areas.push(json!({"id": format!("search_swath-{i}"), "kind": "search_swath", "label": "Searched", "polygon": polygon}));
    }
    for (i, polygon) in cov.uncovered_polygons.iter().enumerate() {
        areas.push(json!({"id": format!("uncovered-{i}"), "kind": "uncovered", "label": "Not yet searched", "polygon": polygon}));
    }
    let label = match cov.percent {
        Some(p) => format!("Search coverage {p:.0}%"),
        None => "Search coverage".to_owned(),
    };
    json!({
        "format": FORMAT,
        "version": 1,
        "active": session.active,
        "session_id": session.session_id,
        "started_ms": session.started_ms,
        "started_by": session.started_by,
        "stopped_ms": session.stopped_ms,
        "stopped_by": session.stopped_by,
        "cleared_ms": session.cleared_ms,
        "cleared_by": session.cleared_by,
        "accepting_uploads": Store::accepting(session, now_ms),
        "late_upload_until_ms": if session.active { None } else { session.stopped_ms.map(|s| s + LATE_UPLOAD_GRACE_S * 1000) },
        "settings": session.settings,
        "limits": {
            "swath_width_m": [SWATH_RANGE_M.start(), SWATH_RANGE_M.end()],
            "max_batch_points": MAX_BATCH_POINTS,
            "max_points_per_device": MAX_POINTS_PER_DEVICE,
            "max_points_total": MAX_POINTS_TOTAL,
            "max_devices": MAX_DEVICES,
            "late_upload_grace_s": LATE_UPLOAD_GRACE_S,
            "max_accuracy_m": MAX_ACCURACY_M,
            "polygon_max_points": POLYGON_MAX_POINTS,
        },
        "target": session.target,
        "stats": {
            "percent_covered": cov.percent,
            "target_m2": cov.target_m2,
            "covered_in_target_m2": cov.covered_in_target_m2,
            "uncovered_m2": cov.uncovered_m2,
            "swath_m2": cov.swath_m2,
            "cell_m": cov.cell_m,
            "target_cell_m": cov.target_cell_m,
            "searchers": list.len(),
            "sharing": searchers.iter().filter(|(_, s, _)| s.sharing && session.active).count(),
            "points": points_total,
            "distance_m": (distance_total * 10.0).round() / 10.0,
        },
        "searchers": list,
        "overlay": {
            "id": format!("search_coverage:{session_tag}"),
            "kind": "search_coverage",
            "label": label,
            "predicted": false,
            "tracks": overlay_tracks,
            "markers": [],
            "areas": areas,
        },
    })
}

// ---------------------------------------------------------------------------
// Requests

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetInput {
    pub polygon: Vec<[f64; 2]>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsUpdate {
    #[serde(default)]
    pub swath_width_m: Option<f64>,
    /// Absent: unchanged. null: remove the target.
    #[serde(default, deserialize_with = "double_option")]
    pub target: Option<Option<TargetInput>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptInRequest {
    device_id: String,
    device_name: String,
    #[serde(default)]
    platform: Option<String>,
    sharing: bool,
    /// With `sharing: false`: also delete this device's uploaded track.
    #[serde(default)]
    delete_my_track: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackPoint {
    seq: u64,
    t_ms: i64,
    lat: f64,
    lon: f64,
    #[serde(default)]
    acc_m: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackBatch {
    v: u32,
    session_id: String,
    opt_in_id: String,
    device_id: String,
    points: Vec<TrackPoint>,
}

fn valid_polygon(polygon: &[[f64; 2]]) -> Result<(), String> {
    if polygon.len() < 3 || polygon.len() > POLYGON_MAX_POINTS {
        return Err(format!("The search area needs 3–{POLYGON_MAX_POINTS} points."));
    }
    if polygon.iter().any(|p| !p[0].is_finite() || !p[1].is_finite() || p[0].abs() > 90.0 || p[1].abs() > 180.0) {
        return Err("Search area points are [lat, lon] in degrees.".into());
    }
    let frame = coverage::Frame::new(polygon[0][0], polygon[0][1]);
    let (mut lo, mut hi) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
    for p in polygon {
        let (x, y) = frame.to_local(p[0], p[1]);
        lo = (lo.0.min(x), lo.1.min(y));
        hi = (hi.0.max(x), hi.1.max(y));
    }
    if (hi.0 - lo.0).hypot(hi.1 - lo.1) > POLYGON_MAX_SPAN_M {
        return Err(format!("The search area may span at most {} km.", POLYGON_MAX_SPAN_M / 1000.0));
    }
    if (hi.0 - lo.0) < 1.0 || (hi.1 - lo.1) < 1.0 {
        return Err("The search area has no extent.".into());
    }
    Ok(())
}

fn valid_source(source: &str) -> bool {
    (1..=32).contains(&source.len()) && source.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

fn make_target(input: TargetInput, actor: &str, now_ms: i64) -> Result<Target, Refusal> {
    valid_polygon(&input.polygon).map_err(|e| refusal(StatusCode::BAD_REQUEST, "invalid_target", e))?;
    let source = input.source.unwrap_or_else(|| SOURCE_MANUAL.into());
    if !valid_source(&source) {
        return Err(refusal(StatusCode::BAD_REQUEST, "invalid_target", "source is 1–32 lowercase letters or '_'."));
    }
    let label = match input.label {
        Some(l) => Some(clean_text(&l, 80).ok_or_else(|| refusal(StatusCode::BAD_REQUEST, "invalid_target", "label is 1–80 characters."))?),
        None => None,
    };
    let polygon = input.polygon.iter().map(|p| [(p[0] * 1e7).round() / 1e7, (p[1] * 1e7).round() / 1e7]).collect();
    Ok(Target { polygon, source, label, set_ms: now_ms, set_by: actor.to_owned() })
}

fn require_commands(principal: &AuthPrincipal) -> Result<String, Refusal> {
    match signed_in(principal) {
        Some(user) if principal.permissions.send_commands || principal.is_admin() => Ok(user),
        _ => Err(refusal(StatusCode::FORBIDDEN, "forbidden", "Command permission is required to run a recovery search.")),
    }
}

fn audit(state: &AppState, text: String, notify: bool) {
    log::info!("{text}");
    if notify {
        crate::web::emit_notification_warning(state, text);
    } else {
        crate::web::persist_message_db_only(state, text);
    }
}

// ---------------------------------------------------------------------------
// Operations (each takes the clock so tests are deterministic)

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRequest {
    #[serde(default)]
    swath_width_m: Option<f64>,
    #[serde(default)]
    target: Option<TargetInput>,
}

pub(crate) fn start(state: &AppState, principal: &AuthPrincipal, body: &[u8], now_ms: i64) -> Result<(), Refusal> {
    let actor = require_commands(principal)?;
    let request: StartRequest = if body.iter().all(u8::is_ascii_whitespace) {
        StartRequest::default()
    } else {
        serde_json::from_slice(body).map_err(|e| refusal(StatusCode::BAD_REQUEST, "invalid", format!("Invalid start request: {e}")))?
    };
    if let Some(w) = request.swath_width_m
        && !SWATH_RANGE_M.contains(&w)
    {
        return Err(refusal(StatusCode::BAD_REQUEST, "invalid", "The swath width must be 2–500 m."));
    }
    let target = request.target.map(|t| make_target(t, &actor, now_ms)).transpose()?;
    {
        let mut inner = state.recovery_search.inner.lock().unwrap();
        if inner.session.active {
            return Err(refusal(StatusCode::CONFLICT, "already_active", "A recovery search is already running."));
        }
        // A new session keeps the configured swath and area, never the old tracks.
        let previous = std::mem::take(&mut inner.session);
        inner.session = Session {
            session_id: Some(format!("rs-{now_ms}-{}", random_hex(4))),
            active: true,
            started_ms: Some(now_ms),
            started_by: Some(actor.clone()),
            settings: Settings { swath_width_m: request.swath_width_m.unwrap_or(previous.settings.swath_width_m) },
            target: target.or(previous.target),
            ..Session::default()
        };
        Store::bump(&mut inner);
    }
    save_now(state);
    audit(state, format!("Recovery search started by {actor}. Searchers may opt in to share their track."), true);
    Ok(())
}

pub(crate) fn stop(state: &AppState, principal: &AuthPrincipal, now_ms: i64) -> Result<(), Refusal> {
    let actor = require_commands(principal)?;
    {
        let mut inner = state.recovery_search.inner.lock().unwrap();
        if !inner.session.active {
            return Err(refusal(StatusCode::CONFLICT, "not_active", "No recovery search is running."));
        }
        inner.session.active = false;
        inner.session.stopped_ms = Some(now_ms);
        inner.session.stopped_by = Some(actor.clone());
        for s in inner.session.searchers.values_mut() {
            s.sharing = false;
        }
        Store::bump(&mut inner);
    }
    save_now(state);
    audit(state, format!("Recovery search stopped by {actor}. Devices stop sharing their track."), true);
    Ok(())
}

pub(crate) fn update_settings(state: &AppState, principal: &AuthPrincipal, body: &[u8], now_ms: i64) -> Result<(), Refusal> {
    let actor = require_commands(principal)?;
    let update: SettingsUpdate = serde_json::from_slice(body)
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "invalid", format!("Invalid recovery search settings: {e}")))?;
    if let Some(w) = update.swath_width_m
        && !SWATH_RANGE_M.contains(&w)
    {
        return Err(refusal(StatusCode::BAD_REQUEST, "invalid", "The swath width must be 2–500 m."));
    }
    let target = match update.target {
        None => None,
        Some(None) => Some(None),
        Some(Some(t)) => Some(Some(make_target(t, &actor, now_ms)?)),
    };
    let mut changes = Vec::new();
    {
        let mut inner = state.recovery_search.inner.lock().unwrap();
        if let Some(w) = update.swath_width_m
            && w != inner.session.settings.swath_width_m
        {
            changes.push(format!("swath {} m → {w} m", inner.session.settings.swath_width_m));
            inner.session.settings.swath_width_m = w;
        }
        if let Some(target) = target {
            changes.push(match &target {
                Some(t) => format!("search area set from {}", t.source),
                None => "search area removed".to_owned(),
            });
            inner.session.target = target;
        }
        if changes.is_empty() {
            return Ok(());
        }
        Store::bump(&mut inner);
    }
    save_now(state);
    audit(state, format!("Recovery search settings changed by {actor}: {}.", changes.join(", ")), false);
    Ok(())
}

/// Sets the predicted landing area from an automatic source (the
/// loss-of-signal prediction, a flight plan). `polygon` is `[lat, lon]`
/// points, a closed ring without the first point repeated. Returns
/// `Ok(false)` when an operator's manual area is kept instead.
pub(crate) fn set_target_area(state: &AppState, polygon: &[[f64; 2]], source: &str) -> Result<bool, String> {
    set_target_area_at(state, polygon, source, now_ms())
}

pub(crate) fn set_target_area_at(state: &AppState, polygon: &[[f64; 2]], source: &str, now_ms: i64) -> Result<bool, String> {
    if !valid_source(source) || source == SOURCE_MANUAL {
        return Err("An automatic source is a lowercase name other than manual.".into());
    }
    let target = make_target(TargetInput { polygon: polygon.to_vec(), source: Some(source.into()), label: None }, "station", now_ms)
        .map_err(|(_, _, text)| text)?;
    {
        let mut inner = state.recovery_search.inner.lock().unwrap();
        match &inner.session.target {
            Some(current) if current.source == SOURCE_MANUAL => return Ok(false),
            Some(current) if current.polygon == target.polygon && current.source == target.source => return Ok(true),
            _ => {}
        }
        inner.session.target = Some(target);
        Store::bump(&mut inner);
    }
    save_now(state);
    Ok(true)
}

pub(crate) fn opt_in(state: &AppState, principal: &AuthPrincipal, body: &[u8], now_ms: i64) -> Result<Value, Refusal> {
    let username = signed_in(principal)
        .ok_or_else(|| refusal(StatusCode::FORBIDDEN, "sign_in", "Sign in to share a search track."))?;
    let request: OptInRequest = serde_json::from_slice(body)
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "invalid", format!("Invalid opt-in request: {e}")))?;
    if !valid_device_id(&request.device_id) {
        return Err(refusal(StatusCode::BAD_REQUEST, "invalid", "device_id must be 1–64 letters, digits, '-' or '_'."));
    }
    let name = clean_text(&request.device_name, NAME_MAX)
        .ok_or_else(|| refusal(StatusCode::BAD_REQUEST, "invalid", format!("device_name is 1–{NAME_MAX} characters.")))?;
    let platform = request.platform.as_deref().and_then(|p| clean_text(p, 16)).unwrap_or_else(|| "web".into());
    let key = key_of(&username, &request.device_id);
    let mut inner = state.recovery_search.inner.lock().unwrap();
    let session_id = inner.session.session_id.clone();
    let text;
    if request.sharing {
        if !inner.session.active {
            return Err(refusal(StatusCode::CONFLICT, "search_off", "Recovery search is off: nothing is shared."));
        }
        if !inner.session.searchers.contains_key(&key) {
            let lower = username.to_lowercase();
            let mine = inner.session.searchers.values().filter(|s| s.username.to_lowercase() == lower).count();
            if mine >= MAX_DEVICES_PER_USER || inner.session.searchers.len() >= MAX_DEVICES {
                return Err(refusal(StatusCode::TOO_MANY_REQUESTS, "too_many_devices", "Too many search devices."));
            }
        }
        let color = inner.session.searchers.len();
        let searcher = inner.session.searchers.entry(key.clone()).or_insert_with(|| Searcher {
            username: username.clone(),
            device_id: request.device_id.clone(),
            name: name.clone(),
            platform: platform.clone(),
            color,
            sharing: false,
            opt_in_id: None,
            opted_in_ms: None,
            opted_out_ms: None,
            points: BTreeMap::new(),
            last_upload_ms: None,
            recent: VecDeque::new(),
        });
        searcher.name = name.clone();
        searcher.platform = platform;
        if !searcher.sharing {
            searcher.sharing = true;
            searcher.opt_in_id = Some(random_hex(12));
            searcher.opted_in_ms = Some(now_ms);
            searcher.opted_out_ms = None;
            text = Some(format!("{name} ({username}) is sharing a recovery search track."));
        } else {
            text = None;
        }
    } else {
        // Opting out is always accepted, in or out of a session.
        text = match inner.session.searchers.get_mut(&key) {
            Some(searcher) => {
                let was = searcher.sharing;
                searcher.sharing = false;
                searcher.opt_in_id = None;
                searcher.opted_out_ms = Some(now_ms);
                if request.delete_my_track {
                    inner.session.searchers.remove(&key);
                    Some(format!("{name} ({username}) stopped sharing and deleted their search track."))
                } else {
                    was.then(|| format!("{name} ({username}) stopped sharing their search track."))
                }
            }
            None => None,
        };
    }
    let searcher = inner.session.searchers.get(&key);
    let reply = json!({
        "session_id": session_id,
        "device_id": request.device_id,
        "sharing": searcher.is_some_and(|s| s.sharing),
        "opt_in_id": searcher.and_then(|s| s.opt_in_id.clone()),
        "opted_in_ms": searcher.and_then(|s| s.opted_in_ms),
        "points": searcher.map_or(0, |s| s.points.len()),
        "max_seq": searcher.and_then(|s| s.points.keys().next_back().copied()),
    });
    Store::bump(&mut inner);
    drop(inner);
    if let Some(text) = text {
        save_now(state);
        audit(state, text, false);
    }
    Ok(reply)
}

pub(crate) fn upload(
    state: &AppState,
    principal: &AuthPrincipal,
    body: &[u8],
    now: Instant,
    now_ms: i64,
) -> Result<Value, Refusal> {
    let username = signed_in(principal)
        .ok_or_else(|| refusal(StatusCode::FORBIDDEN, "sign_in", "Sign in to share a search track."))?;
    let batch: TrackBatch = serde_json::from_slice(body)
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "invalid", format!("Invalid track batch: {e}")))?;
    if batch.v != 1 {
        return Err(refusal(StatusCode::BAD_REQUEST, "invalid", "Unsupported track batch version."));
    }
    if batch.points.len() > MAX_BATCH_POINTS {
        return Err(refusal(StatusCode::PAYLOAD_TOO_LARGE, "batch_too_large", format!("At most {MAX_BATCH_POINTS} points per batch.")));
    }
    let key = key_of(&username, &batch.device_id);
    let mut inner = state.recovery_search.inner.lock().unwrap();
    let session = &mut inner.session;
    if !Store::accepting(session, now_ms) {
        return Err(refusal(StatusCode::CONFLICT, "search_off", "Recovery search is off: the track was not accepted."));
    }
    if session.session_id.as_deref() != Some(batch.session_id.as_str()) {
        return Err(refusal(StatusCode::CONFLICT, "session_mismatch", "This track belongs to another search session."));
    }
    let stopped_ms = session.stopped_ms.filter(|_| !session.active);
    let total: usize = session.searchers.values().map(|s| s.points.len()).sum();
    let Some(searcher) = session.searchers.get_mut(&key) else {
        return Err(refusal(StatusCode::FORBIDDEN, "not_opted_in", "This device has not opted in to share its track."));
    };
    // After a stop the last opt-in id stays valid for the late-upload grace.
    let id_ok = searcher.opt_in_id.as_deref() == Some(batch.opt_in_id.as_str()) && (searcher.sharing || stopped_ms.is_some());
    if !id_ok {
        return Err(refusal(StatusCode::FORBIDDEN, "not_opted_in", "This device has not opted in to share its track."));
    }
    while searcher.recent.front().is_some_and(|t| now.saturating_duration_since(*t) > RATE_WINDOW) {
        searcher.recent.pop_front();
    }
    if searcher.recent.len() >= RATE_MAX {
        return Err(refusal(StatusCode::TOO_MANY_REQUESTS, "rate_limited", format!("At most {RATE_MAX} batches per minute.")));
    }
    searcher.recent.push_back(now);
    let earliest = searcher.opted_in_ms.unwrap_or(i64::MAX) - CLOCK_SKEW_MS;
    let latest = stopped_ms.map_or(now_ms, |s| s.min(now_ms)) + CLOCK_SKEW_MS;
    let (mut accepted, mut duplicates, mut rejected, mut full) = (0usize, 0usize, 0usize, false);
    let mut room = MAX_POINTS_TOTAL.saturating_sub(total);
    for p in batch.points {
        if searcher.points.contains_key(&p.seq) {
            duplicates += 1;
            continue;
        }
        let valid = p.lat.is_finite()
            && p.lon.is_finite()
            && p.lat.abs() <= 90.0
            && p.lon.abs() <= 180.0
            && !(p.lat == 0.0 && p.lon == 0.0)
            && p.acc_m.is_none_or(|a| a.is_finite() && (0.0..=MAX_ACCURACY_M).contains(&a))
            && (earliest..=latest).contains(&p.t_ms);
        if !valid {
            rejected += 1;
            continue;
        }
        if searcher.points.len() >= MAX_POINTS_PER_DEVICE || room == 0 {
            full = true;
            rejected += 1;
            continue;
        }
        searcher.points.insert(p.seq, Point { t_ms: p.t_ms, lat: p.lat, lon: p.lon, acc_m: p.acc_m.map(|a| a as f32) });
        room -= 1;
        accepted += 1;
    }
    searcher.last_upload_ms = Some(now_ms);
    let reply = json!({
        "accepted": accepted,
        "duplicates": duplicates,
        "rejected": rejected,
        "full": full,
        "stored_points": searcher.points.len(),
        "max_seq": searcher.points.keys().next_back().copied(),
    });
    if accepted > 0 {
        Store::bump(&mut inner);
    }
    Ok(reply)
}

pub(crate) fn clear_tracks(state: &AppState, principal: &AuthPrincipal, now_ms: i64) -> Result<(), Refusal> {
    if principal.anonymous || !principal.is_admin() {
        return Err(refusal(StatusCode::FORBIDDEN, "forbidden", "Only an administrator may clear search tracks."));
    }
    let actor = principal.username.clone().unwrap_or_default();
    let removed = {
        let mut inner = state.recovery_search.inner.lock().unwrap();
        let mut removed = 0;
        for s in inner.session.searchers.values_mut() {
            removed += s.points.len();
            s.points.clear();
        }
        // Devices no longer sharing are forgotten entirely.
        inner.session.searchers.retain(|_, s| s.sharing);
        inner.session.cleared_ms = Some(now_ms);
        inner.session.cleared_by = Some(actor.clone());
        Store::bump(&mut inner);
        removed
    };
    save_now(state);
    audit(state, format!("Recovery search tracks cleared by {actor} ({removed} fixes)."), false);
    Ok(())
}

/// The caller's own device state (`?device_id=`), so a reloaded client can
/// resume sharing without asking again.
fn you(state: &AppState, principal: &AuthPrincipal, device_id: Option<&str>) -> Value {
    let (Some(user), Some(device_id)) = (signed_in(principal), device_id.filter(|d| valid_device_id(d))) else {
        return Value::Null;
    };
    let inner = state.recovery_search.inner.lock().unwrap();
    let searcher = inner.session.searchers.get(&key_of(&user, device_id));
    json!({
        "device_id": device_id,
        "id": public_id(&key_of(&user, device_id)),
        "sharing": searcher.is_some_and(|s| s.sharing && inner.session.active),
        "opt_in_id": searcher.filter(|s| s.sharing || !inner.session.active).and_then(|s| s.opt_in_id.clone()),
        "points": searcher.map_or(0, |s| s.points.len()),
        "max_seq": searcher.and_then(|s| s.points.keys().next_back().copied()),
    })
}

pub(crate) fn status(state: &AppState, principal: &AuthPrincipal, device_id: Option<&str>) -> Value {
    let mut body = state.recovery_search.status_body(now_ms());
    body["you"] = you(state, principal, device_id);
    body
}

/// The session for a replay bundle: tracks trimmed to `[from_ms, to_ms]`
/// (when given). None when nothing was recorded in that window.
pub(crate) fn export(state: &AppState, from_ms: Option<i64>, to_ms: Option<i64>) -> Option<Value> {
    let mut session = state.recovery_search.inner.lock().unwrap().session.clone();
    session.session_id.as_ref()?;
    for s in session.searchers.values_mut() {
        s.points.retain(|_, p| from_ms.is_none_or(|f| p.t_ms >= f) && to_ms.is_none_or(|t| p.t_ms <= t));
        s.opt_in_id = None;
    }
    session.searchers.retain(|_, s| !s.points.is_empty());
    if session.searchers.is_empty() {
        return None;
    }
    let mut body = build_status(&session, session.stopped_ms.or(session.started_ms).unwrap_or(0));
    body["accepting_uploads"] = false.into();
    body["late_upload_until_ms"] = Value::Null;
    body["window"] = json!({"from_ms": from_ms, "to_ms": to_ms});
    Some(body)
}

// ---------------------------------------------------------------------------
// Persistence

fn save(inner: &mut Inner) -> std::io::Result<()> {
    use std::io::Write;
    let Some(dir) = inner.data_dir.clone() else { return Ok(()) };
    let path = dir.join(FILE);
    if inner.session.session_id.is_none() && inner.session.target.is_none() {
        let _ = std::fs::remove_file(&path);
        inner.saved_revision = inner.revision;
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = path.with_extension("pending");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(&inner.session).unwrap_or_default())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    inner.saved_revision = inner.revision;
    Ok(())
}

fn save_now(state: &AppState) {
    let mut inner = state.recovery_search.inner.lock().unwrap();
    if let Err(error) = save(&mut inner) {
        log::warn!("Recovery search state was not saved: {error}");
    }
}

fn read(dir: &Path) -> Session {
    match std::fs::read(dir.join(FILE)) {
        Ok(bytes) => serde_json::from_slice::<Session>(&bytes).unwrap_or_else(|_| {
            log::warn!("Ignoring unreadable {FILE}.");
            Session::default()
        }),
        Err(_) => Session::default(),
    }
}

pub(crate) fn load(state: &AppState, dir: PathBuf) {
    let session = read(&dir);
    let mut inner = state.recovery_search.inner.lock().unwrap();
    inner.session = session;
    inner.data_dir = Some(dir);
    Store::bump(&mut inner);
    inner.saved_revision = inner.revision;
}

/// Saves uploaded fixes every few seconds (start, stop and settings save at once).
pub(crate) fn spawn_tick(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut shutdown = state.shutdown_subscribe();
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.recv() => {
                    save_if_dirty(&state);
                    break;
                }
            }
            save_if_dirty(&state);
        }
    });
}

fn save_if_dirty(state: &AppState) {
    let mut inner = state.recovery_search.inner.lock().unwrap();
    if inner.revision != inner.saved_revision
        && let Err(error) = save(&mut inner)
    {
        log::warn!("Recovery search state was not saved: {error}");
    }
}

// ---------------------------------------------------------------------------
// Routes

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/recovery_search", get(get_status))
        .route("/api/recovery_search/export", get(get_export))
        .route("/api/recovery_search/start", post(post_start))
        .route("/api/recovery_search/stop", post(post_stop))
        .route("/api/recovery_search/settings", put(put_settings))
        .route("/api/recovery_search/opt_in", post(post_opt_in))
        .route("/api/recovery_search/track", post(post_track))
        .route("/api/recovery_search/tracks", delete(delete_tracks))
}

fn no_store(body: Value) -> Response {
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn error_response((status, code, text): Refusal) -> Response {
    (status, Json(json!({"error": text, "code": code}))).into_response()
}

#[derive(Debug, Default, Deserialize)]
struct StatusQuery {
    device_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ExportQuery {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
}

async fn principal_of(state: &Arc<AppState>, headers: &HeaderMap) -> Result<AuthPrincipal, Response> {
    crate::web::authorize_headers(state, headers, Permission::ViewData).await
}

async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<StatusQuery>) -> Response {
    let principal = match principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let task_state = state.clone();
    match tokio::task::spawn_blocking(move || status(&task_state, &principal, query.device_id.as_deref())).await {
        Ok(body) => no_store(body),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn get_export(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<ExportQuery>) -> Response {
    if let Err(response) = principal_of(&state, &headers).await {
        return response;
    }
    match export(&state, query.from_ms, query.to_ms) {
        Some(body) => no_store(body),
        None => error_response(refusal(StatusCode::NOT_FOUND, "no_tracks", "No search tracks in that window.")),
    }
}

/// Every mutating route answers with the fresh status document.
async fn mutate(
    state: Arc<AppState>,
    headers: HeaderMap,
    op: impl FnOnce(&AppState, &AuthPrincipal) -> Result<Option<Value>, Refusal> + Send + 'static,
) -> Response {
    let principal = match principal_of(&state, &headers).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    match op(&state, &principal) {
        Ok(Some(reply)) => no_store(reply),
        Ok(None) => {
            let task_state = state.clone();
            match tokio::task::spawn_blocking(move || status(&task_state, &principal, None)).await {
                Ok(body) => no_store(body),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        Err(refused) => error_response(refused),
    }
}

async fn post_start(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    mutate(state, headers, move |s, p| start(s, p, &body, now_ms()).map(|_| None)).await
}

async fn post_stop(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    mutate(state, headers, move |s, p| stop(s, p, now_ms()).map(|_| None)).await
}

async fn put_settings(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    mutate(state, headers, move |s, p| update_settings(s, p, &body, now_ms()).map(|_| None)).await
}

async fn post_opt_in(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    mutate(state, headers, move |s, p| opt_in(s, p, &body, now_ms()).map(Some)).await
}

async fn post_track(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    mutate(state, headers, move |s, p| upload(s, p, &body, Instant::now(), now_ms()).map(Some)).await
}

async fn delete_tracks(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    mutate(state, headers, move |s, p| clear_tracks(s, p, now_ms()).map(|_| None)).await
}
