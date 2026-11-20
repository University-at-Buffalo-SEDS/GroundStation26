//! The FC launch site (docs/integrated-ground-test.md sections 6.1b and 7.4).
//!
//! An administrator sets the site (lat_deg, lon_deg, alt_msl_m) or resets it
//! to the FC's compiled default. The station persists it in its data
//! directory and sends it to the FC as `LAUNCH_SITE` (Static Float32 x 3)
//! when the setting changes, when the FC is first seen or reboots (its
//! BOARD_HEALTH uptime goes backwards, as for the board config resyncs in
//! crate::board_config_sync), and every 10 s until the FC's `LAUNCH_SITE`
//! echo matches. With no setting nothing is sent and the FC keeps its
//! compiled site. The FC applies a site only on the pad (no Launch request,
//! Armed or earlier, no replay); it echoes the site it uses either way, so the
//! station stops resending once Launch is latched or committed.
use crate::auth::Permission;
use crate::state::AppState;
use crate::types::FlightState;
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FILE: &str = "fc_launch_site.json";
/// Resend period while the FC's echo does not match.
pub(crate) const RESEND: Duration = Duration::from_secs(10);
/// Echo comparison tolerance, on the float32 values both sides use.
const TOLERANCE: f64 = 1e-5;
/// The FC's compiled `LAUNCH_SITE_*` (FlightComputer26 fcconfig.h).
pub(crate) const COMPILED_DEFAULT: Site = Site { lat_deg: 31.758, lon_deg: -102.046, alt_msl_m: 890.016 };

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub alt_msl_m: f64,
}

impl Site {
    /// The FC's acceptance limits (doc section 6.1b).
    fn validate(&self) -> Result<(), String> {
        let finite = self.lat_deg.is_finite() && self.lon_deg.is_finite() && self.alt_msl_m.is_finite();
        if !finite {
            return Err("Launch site values must be finite numbers.".into());
        }
        if self.lat_deg.abs() > 90.0 || self.lon_deg.abs() > 180.0 {
            return Err("Latitude must be within ±90° and longitude within ±180°.".into());
        }
        if !(-500.0..=9000.0).contains(&self.alt_msl_m) {
            return Err("Altitude must be within -500..9000 m MSL.".into());
        }
        Ok(())
    }
    /// The float32 payload sent to the FC.
    pub(crate) fn payload(&self) -> Vec<u8> {
        [self.lat_deg as f32, self.lon_deg as f32, self.alt_msl_m as f32]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }
    fn from_values(values: &[Option<f32>]) -> Option<Self> {
        match values {
            [Some(lat), Some(lon), Some(alt)] if lat.is_finite() && lon.is_finite() && alt.is_finite() => {
                Some(Self { lat_deg: f64::from(*lat), lon_deg: f64::from(*lon), alt_msl_m: f64::from(*alt) })
            }
            _ => None,
        }
    }
    /// Equal as the FC holds them (float32), within the tolerance.
    pub(crate) fn matches(&self, other: &Site) -> bool {
        let same = |a: f64, b: f64| (f64::from(a as f32) - f64::from(b as f32)).abs() <= TOLERANCE;
        same(self.lat_deg, other.lat_deg) && same(self.lon_deg, other.lon_deg) && same(self.alt_msl_m, other.alt_msl_m)
    }
}

#[derive(Default)]
struct Inner {
    configured: Option<Site>,
    data_dir: Option<PathBuf>,
    /// The FC's last echo; cleared when the FC reboots (it returns to its
    /// compiled site).
    fc_reported: Option<Site>,
    /// The FC's uptime in its last BOARD_HEALTH; none before it is seen.
    fc_uptime_ms: Option<u64>,
    /// Send at the next tick: the setting changed, or the FC was first seen
    /// or rebooted.
    due: bool,
    last_sent: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Runtime {
    inner: Mutex<Inner>,
}

fn tracker(state: &AppState) -> &Runtime {
    &state.igt.launch_site
}

pub(crate) fn load(state: &AppState, dir: PathBuf) {
    let configured = std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Option<Site>>(&bytes).ok())
        .flatten()
        .filter(|site| site.validate().is_ok());
    let mut inner = tracker(state).inner.lock().unwrap();
    inner.configured = configured;
    inner.data_dir = Some(dir);
    inner.due = configured.is_some();
}

fn save(dir: &Path, site: Option<Site>) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE);
    let tmp = path.with_extension("pending");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(&site).unwrap_or_default())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

/// An FC BOARD_HEALTH arrived: the first one, or an uptime that went
/// backwards (a reboot), makes the site due.
pub(crate) fn observe_fc_health(state: &AppState, words: &[u32]) {
    if words.len() != 24 || words[4] == 0 {
        return;
    }
    let ticks = u64::from(words[2]) | (u64::from(words[3]) << 32);
    let uptime_ms = ticks.saturating_mul(1000) / u64::from(words[4]);
    let mut inner = tracker(state).inner.lock().unwrap();
    let rebooted = inner.fc_uptime_ms.is_some_and(|old| uptime_ms < old);
    if inner.fc_uptime_ms.is_none() || rebooted {
        inner.due = true;
    }
    if rebooted {
        inner.fc_reported = None;
    }
    inner.fc_uptime_ms = Some(uptime_ms);
}

/// The FC's `LAUNCH_SITE` echo arrived.
pub(crate) fn observe_echo(state: &AppState, values: &[Option<f32>]) {
    if let Some(site) = Site::from_values(values) {
        tracker(state).inner.lock().unwrap().fc_reported = Some(site);
    }
}

fn launched(state: &AppState) -> bool {
    use std::sync::atomic::Ordering;
    state.launch_indicator_latched()
        || state.launch_sequence_command_pending.load(Ordering::Acquire)
        || (state.local_flight_state_snapshot() as u8) >= FlightState::Launch as u8
            && state.local_flight_state_snapshot() != FlightState::Aborted
}

/// What to send now, if anything.
fn due(inner: &mut Inner, now: Instant, launched: bool) -> Option<Site> {
    let site = inner.configured?;
    if inner.fc_uptime_ms.is_none() || launched {
        return None;
    }
    if inner.fc_reported.is_some_and(|reported| reported.matches(&site)) {
        inner.due = false;
        return None;
    }
    let periodic = inner.last_sent.is_none_or(|at| now.saturating_duration_since(at) >= RESEND);
    if !(inner.due || periodic) {
        return None;
    }
    inner.due = false;
    inner.last_sent = Some(now);
    Some(site)
}

fn send(state: &AppState, site: &Site) -> Result<(), String> {
    let router = state.topology_router.get().ok_or("The station network is not running.")?;
    let queued = crate::gse::queue_controller_with_stop_fence(
        state,
        router,
        crate::telemetry_schema::data_type("LAUNCH_SITE"),
        &site.payload(),
    )
    .map_err(|error| format!("{error:?}"))?;
    if !queued {
        return Err("Manual Stop is pending".into());
    }
    crate::telemetry_task::flush_command_tx(router, "LAUNCH_SITE tx");
    Ok(())
}

/// Every second (board config sync service): send the site when due.
pub(crate) fn tick(state: &AppState, now: Instant) {
    if crate::rrls::is_rrls() {
        return;
    }
    let launched = launched(state);
    let Some(site) = due(&mut tracker(state).inner.lock().unwrap(), now, launched) else {
        return;
    };
    match send(state, &site) {
        Ok(()) => log::info!(
            "LAUNCH_SITE sent to the FC: {:.6}, {:.6}, {:.1} m",
            site.lat_deg,
            site.lon_deg,
            site.alt_msl_m
        ),
        Err(error) => log::warn!("LAUNCH_SITE was not sent to the FC: {error}"),
    }
}

/// `GET /api/fc/launch-site`.
pub(crate) fn view(state: &AppState) -> serde_json::Value {
    let inner = tracker(state).inner.lock().unwrap();
    let target = inner.configured.unwrap_or(COMPILED_DEFAULT);
    let confirmed = inner.fc_reported.is_some_and(|reported| reported.matches(&target));
    json!({
        "configured": inner.configured,
        "compiled_default": COMPILED_DEFAULT,
        "fc_reported": inner.fc_reported,
        "confirmed": confirmed,
        "pending": inner.configured.is_some() && !confirmed,
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Request {
    Reset { reset: bool },
    Set(Site),
}

/// `PUT /api/fc/launch-site`: a site, or `{reset: true}`; admin only.
pub(crate) fn update(
    state: &AppState,
    principal: &crate::auth::AuthPrincipal,
    body: &[u8],
) -> Result<serde_json::Value, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Only an administrator may set the launch site.".into()));
    }
    if crate::rrls::is_rrls() {
        return Err((StatusCode::FORBIDDEN, "The launch site is set only on the primary GroundStation.".into()));
    }
    let bad = || {
        (
            StatusCode::BAD_REQUEST,
            "Expected {lat_deg, lon_deg, alt_msl_m} or {reset: true}.".to_string(),
        )
    };
    let site = match serde_json::from_slice::<Request>(body).map_err(|_| bad())? {
        Request::Reset { reset: true } => None,
        Request::Reset { reset: false } => return Err(bad()),
        Request::Set(site) => {
            site.validate().map_err(|error| (StatusCode::BAD_REQUEST, error))?;
            Some(site)
        }
    };
    {
        let mut inner = tracker(state).inner.lock().unwrap();
        if let Some(dir) = &inner.data_dir {
            save(dir, site).map_err(|error| {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("The launch site was not saved: {error}"))
            })?;
        }
        inner.configured = site;
        inner.due = site.is_some();
    }
    let actor = principal.username.clone().unwrap_or_default();
    let text = match site {
        Some(site) => format!(
            "FC launch site set by {actor}: {:.6}, {:.6}, {:.1} m MSL.",
            site.lat_deg, site.lon_deg, site.alt_msl_m
        ),
        None => format!(
            "FC launch site reset by {actor}: the FC keeps its compiled site (a site already sent stays on the FC until it resets)."
        ),
    };
    log::info!("{text}");
    crate::web::persist_message_db_only(state, text);
    tick(state, Instant::now());
    Ok(view(state))
}

async fn get_site(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(view(&state))).into_response()
}

async fn put_site(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    // Sending may wait out a briefly held SDK core.
    match tokio::task::spawn_blocking(move || update(&state, &principal, &body)).await {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err((status, error))) => (status, Json(json!({"error": error}))).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "Launch site update failed."}))).into_response(),
    }
}

pub(crate) fn routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new().route("/api/fc/launch-site", axum::routing::get(get_site).put(put_site))
}

#[cfg(test)]
pub(crate) fn last_sent_for_test(state: &AppState) -> Option<Instant> {
    tracker(state).inner.lock().unwrap().last_sent
}

/// The scheduling decision alone (no packet): the site due now, if any.
#[cfg(test)]
pub(crate) fn due_for_test(state: &AppState, now: Instant, launched: bool) -> Option<Site> {
    due(&mut tracker(state).inner.lock().unwrap(), now, launched)
}

#[cfg(test)]
mod tests;
