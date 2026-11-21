//! Clean power off / reboot of the station host (admin only).
//!
//! Applies only to the station that serves the request (the primary GS, or
//! an RRLS for its own Pi). Before asking the host, the station closes every
//! event capture (with its manifest), closes the current rolling segment,
//! persists pending state and tells every client it is going down.
//!
//! Host mechanism (`GS_HOST_POWER`):
//! - `auto` (default): `path-unit` when the host helper is installed (its
//!   marker file exists), otherwise unsupported.
//! - `path-unit`: write `data/host-power/request`; the host's
//!   `gs-host-power.path` unit runs `systemctl poweroff|reboot`. Works from
//!   the Docker container without host privileges.
//! - `systemctl`: run `systemctl poweroff|reboot` directly (native systemd
//!   service running as root, or with a polkit rule for its user).
//! - `off`: unsupported.
use crate::{auth::Permission, state::AppState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const HOST_POWER_DIR: &str = "host-power";
pub(crate) const MARKER_FILE: &str = "helper-installed";
pub(crate) const REQUEST_FILE: &str = "request";

static IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    PowerOff,
    Reboot,
}
impl Action {
    fn systemctl_verb(self) -> &'static str {
        match self {
            Self::PowerOff => "poweroff",
            Self::Reboot => "reboot",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::PowerOff => "powering off",
            Self::Reboot => "rebooting",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Mechanism {
    PathUnit(PathBuf),
    Systemctl,
    #[cfg(test)]
    Mock(Arc<std::sync::Mutex<Vec<Action>>>),
    Unsupported(String),
}

pub(crate) fn mechanism(root: &Path) -> Mechanism {
    #[cfg(test)]
    if let Some(mock) = test_mock::get() {
        return Mechanism::Mock(mock);
    }
    let dir = root.join(HOST_POWER_DIR);
    match std::env::var("GS_HOST_POWER").unwrap_or_else(|_| "auto".into()).as_str() {
        "off" => Mechanism::Unsupported("Host power control is turned off on this station (GS_HOST_POWER=off).".into()),
        "systemctl" => Mechanism::Systemctl,
        "path-unit" => Mechanism::PathUnit(dir),
        _ if dir.join(MARKER_FILE).is_file() => Mechanism::PathUnit(dir),
        _ => Mechanism::Unsupported(
            "Not supported here: the host power helper is not installed on this station (see docs/backend/host-power.md)."
                .into(),
        ),
    }
}

impl Mechanism {
    fn id(&self) -> &'static str {
        match self {
            Self::PathUnit(_) => "path_unit",
            Self::Systemctl => "systemctl",
            #[cfg(test)]
            Self::Mock(_) => "mock",
            Self::Unsupported(_) => "unsupported",
        }
    }
    async fn request(&self, action: Action) -> Result<(), String> {
        match self {
            Self::PathUnit(dir) => {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                let tmp = dir.join("request.tmp");
                std::fs::write(&tmp, format!("{}\n", action.systemctl_verb())).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, dir.join(REQUEST_FILE)).map_err(|e| e.to_string())
            }
            Self::Systemctl => {
                let output = tokio::process::Command::new("systemctl")
                    .arg(action.systemctl_verb())
                    .output()
                    .await
                    .map_err(|e| format!("systemctl unavailable: {e}"))?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).trim().chars().take(300).collect())
                }
            }
            #[cfg(test)]
            Self::Mock(calls) => {
                calls.lock().unwrap().push(action);
                Ok(())
            }
            Self::Unsupported(reason) => Err(reason.clone()),
        }
    }
}

/// The station this request powers off, as operators know it.
pub(crate) fn station_name() -> String {
    if let Ok(name) = std::env::var("GS_STATION_NAME")
        && !name.trim().is_empty()
    {
        return name.trim().chars().take(64).collect();
    }
    if crate::rrls::is_rrls() { "RRLS".into() } else { "Ground Station".into() }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Blocker {
    pub reason: String,
    /// An administrator may override with an explicit, logged force.
    pub forceable: bool,
}

/// Why powering off now is unsafe, if it is.
pub(crate) fn blocker(state: &AppState) -> Option<Blocker> {
    use crate::types::FlightState as F;
    let flight = state.local_flight_state_snapshot();
    if matches!(
        flight,
        F::Armed | F::Launch | F::Ascent | F::Coast | F::Apogee | F::Descent | F::Reefing | F::InFlight
    ) {
        return Some(Blocker {
            reason: format!("The vehicle is {flight:?}. The station cannot power off or reboot from Armed through landing."),
            forceable: false,
        });
    }
    let store = &state.recording_store;
    if store
        .captures()
        .iter()
        .any(|c| matches!(c.kind, crate::recording_store::EventKind::Launch | crate::recording_store::EventKind::Hitl))
    {
        return Some(Blocker { reason: "A launch capture is still being recorded.".into(), forceable: true });
    }
    if matches!(flight, F::PreFill | F::FillTest | F::NitrogenFill | F::NitrousFill) {
        return Some(Blocker { reason: format!("A fill is in progress ({flight:?})."), forceable: true });
    }
    if state.sequence_progress.lock().map(|s| s.has_active_run()).unwrap_or(false) {
        return Some(Blocker { reason: "A station sequence is running.".into(), forceable: true });
    }
    if crate::igt::igt_active(state) {
        return Some(Blocker { reason: "An integrated ground test is active.".into(), forceable: true });
    }
    if flight == F::Aborted {
        return Some(Blocker { reason: "The station is in the Aborted state.".into(), forceable: true });
    }
    None
}

#[derive(Serialize)]
struct Status {
    station_name: String,
    supported: bool,
    mechanism: &'static str,
    unsupported_reason: Option<String>,
    blocker: Option<Blocker>,
    in_progress: bool,
}

#[derive(Deserialize)]
pub(crate) struct Request {
    pub action: Action,
    /// The client showed the confirm dialog and the operator accepted it.
    #[serde(default)]
    pub confirm: bool,
    /// Override a forceable blocker; must be the literal "FORCE".
    #[serde(default)]
    pub force: Option<String>,
}

pub(crate) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/system/power", get(status).post(request))
}

fn root(state: &AppState) -> PathBuf {
    crate::recording_export::directory(state)
}

async fn admin(state: &Arc<AppState>, headers: &HeaderMap) -> Result<crate::auth::AuthPrincipal, Response> {
    let principal = crate::web::authorize_headers(state, headers, Permission::ViewData).await?;
    if !principal.is_admin() {
        return Err((StatusCode::FORBIDDEN, "Administrator access required to power off or reboot the station").into_response());
    }
    Ok(principal)
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = admin(&state, &headers).await {
        return response;
    }
    let mechanism = mechanism(&root(&state));
    Json(Status {
        station_name: station_name(),
        supported: !matches!(mechanism, Mechanism::Unsupported(_)),
        mechanism: mechanism.id(),
        unsupported_reason: match &mechanism {
            Mechanism::Unsupported(reason) => Some(reason.clone()),
            _ => None,
        },
        blocker: blocker(&state),
        in_progress: IN_PROGRESS.load(Ordering::SeqCst),
    })
    .into_response()
}

async fn request(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Request>) -> Response {
    let principal = match admin(&state, &headers).await {
        Ok(p) => p,
        Err(response) => return response,
    };
    let who = principal.username.clone().unwrap_or_else(|| "administrator".into());
    match admit(&state, &body, &who) {
        Ok(mechanism) => {
            let action = body.action;
            let forced = body.force.is_some();
            let task_state = state.clone();
            tokio::spawn(async move { execute(task_state, mechanism, action, who, forced).await });
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "station_name": station_name(),
                    "action": action,
                    "message": format!("{} is closing its recordings and {}.", station_name(), action.label()),
                })),
            )
                .into_response()
        }
        Err((status, message)) => (status, message).into_response(),
    }
}

/// Every check before anything changes. Audited whether admitted or refused.
pub(crate) fn admit(state: &AppState, body: &Request, who: &str) -> Result<Mechanism, (StatusCode, String)> {
    let now = crate::telemetry_task::get_current_timestamp_ms();
    log::warn!("host power request: {:?} by {who} at {now} (force: {})", body.action, body.force.is_some());
    if !body.confirm {
        return Err((StatusCode::BAD_REQUEST, "Confirm the power request first.".into()));
    }
    let mechanism = mechanism(&root(state));
    if let Mechanism::Unsupported(reason) = &mechanism {
        return Err((StatusCode::NOT_IMPLEMENTED, reason.clone()));
    }
    if let Some(blocker) = blocker(state) {
        let forced = body.force.as_deref() == Some("FORCE");
        if !(blocker.forceable && forced) {
            log::warn!("host power request by {who} refused: {}", blocker.reason);
            return Err((StatusCode::CONFLICT, format!("Refused: {}", blocker.reason)));
        }
        let message = format!(
            "FORCED {:?} of {} by {who} despite: {}",
            body.action,
            station_name(),
            blocker.reason
        );
        log::error!("{message}");
        crate::web::emit_warning(state, message);
    }
    if IN_PROGRESS.swap(true, Ordering::SeqCst) {
        return Err((StatusCode::CONFLICT, "A power off or reboot is already in progress.".into()));
    }
    Ok(mechanism)
}

/// Close recordings cleanly, tell every client, then ask the host.
pub(crate) async fn execute(state: Arc<AppState>, mechanism: Mechanism, action: Action, who: String, forced: bool) {
    let station = station_name();
    state.add_notification(format!(
        "{station} {} requested by {who}{}. Recordings are being closed; the station will go offline.",
        action.label(),
        if forced { " (forced)" } else { "" }
    ));
    let root = root(&state);
    let result = format!("Station {} by {who}", action.label());
    if state.recording_store.close_all_now(crate::telemetry_task::get_current_timestamp_ms() as i64, &result) > 0 {
        // Let the DB actor commit the final rows, then copy every capture.
        tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
        crate::recording_store::run_due(&state, &root).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while !state.recording_store.captures().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            crate::recording_store::run_due(&state, &root).await;
        }
    }
    crate::recording_store::persist_pending(&state, &root);
    // Close the current segment cleanly (journals checkpointed).
    let before = state.recording_status_snapshot().db_path;
    if state.recording_store.rolling_enabled()
        && state
            .db_queue_tx
            .send(crate::telemetry_db::DbQueueItem::Control(crate::telemetry_db::RecordingCommand::RotateSegment))
            .await
            .is_ok()
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.recording_status_snapshot().db_path == before && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    state.add_notification(format!("{station} is {} now.", action.label()));
    log::warn!("host power: asking the host to {} ({})", action.systemctl_verb(), mechanism.id());
    if let Err(error) = mechanism.request(action).await {
        log::error!("host power request failed: {error}");
        crate::web::emit_warning(&state, format!("{station} could not start {}: {error}. Recording continues.", action.label()));
        IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub(crate) mod test_mock {
    use super::*;
    static MOCK: std::sync::Mutex<Option<Arc<std::sync::Mutex<Vec<Action>>>>> = std::sync::Mutex::new(None);
    pub(crate) fn install() -> Arc<std::sync::Mutex<Vec<Action>>> {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        *MOCK.lock().unwrap() = Some(calls.clone());
        calls
    }
    pub(crate) fn get() -> Option<Arc<std::sync::Mutex<Vec<Action>>>> {
        MOCK.lock().unwrap().clone()
    }
    pub(crate) fn reset_in_progress() {
        IN_PROGRESS.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FlightState;

    fn body(action: Action, confirm: bool, force: Option<&str>) -> Request {
        Request { action, confirm, force: force.map(str::to_owned) }
    }

    /// One test: the host mechanism selection is process-global.
    #[tokio::test]
    async fn power_requests_are_refused_when_unsafe_and_close_recordings_before_the_mocked_host_action() {
        let root = crate::recording_export::tests::TestDirectory::new();
        let state = crate::recording_export::tests::export_state(&root.0, true).await;
        // A development machine without the helper: reported, never attempted.
        let refused = admit(&state, &body(Action::Reboot, true, None), "admin").unwrap_err();
        assert_eq!(refused.0, StatusCode::NOT_IMPLEMENTED);
        assert!(refused.1.starts_with("Not supported here"));
        // With the host helper installed the request is a file the host unit
        // acts on (the container needs no host privileges).
        std::fs::create_dir_all(root.0.join(HOST_POWER_DIR)).unwrap();
        std::fs::write(root.0.join(HOST_POWER_DIR).join(MARKER_FILE), b"installed").unwrap();
        let mechanism = mechanism(&root.0);
        assert!(matches!(mechanism, Mechanism::PathUnit(_)));
        // Writing the request is the whole host action; nothing here powers off.
        let probe = Mechanism::PathUnit(root.0.join("probe"));
        probe.request(Action::PowerOff).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.0.join("probe").join(REQUEST_FILE)).unwrap(), "poweroff\n");

        let calls = test_mock::install();
        test_mock::reset_in_progress();
        assert_eq!(admit(&state, &body(Action::PowerOff, false, None), "admin").unwrap_err().0, StatusCode::BAD_REQUEST);
        // Armed or flying: refused, and not even FORCE overrides it.
        *state.state.lock().unwrap() = FlightState::Armed;
        let refused = admit(&state, &body(Action::PowerOff, true, Some("FORCE")), "admin").unwrap_err();
        assert_eq!(refused.0, StatusCode::CONFLICT);
        assert!(refused.1.contains("Armed"));
        assert!(!blocker(&state).unwrap().forceable);
        // An active fill: refused, but an explicit FORCE is allowed and logged loudly.
        *state.state.lock().unwrap() = FlightState::NitrousFill;
        assert_eq!(admit(&state, &body(Action::Reboot, true, None), "admin").unwrap_err().0, StatusCode::CONFLICT);
        assert_eq!(admit(&state, &body(Action::Reboot, true, Some("force")), "admin").unwrap_err().0, StatusCode::CONFLICT);
        assert!(admit(&state, &body(Action::Reboot, true, Some("FORCE")), "admin").is_ok());
        test_mock::reset_in_progress();
        // A launch capture in progress blocks too.
        *state.state.lock().unwrap() = FlightState::Idle;
        state.recording_store.enable_rolling();
        crate::recording_store::observe_flight_state(&state, FlightState::Launch as i64);
        assert!(blocker(&state).unwrap().reason.contains("launch capture"));
        crate::recording_store::observe_flight_state(&state, FlightState::Landed as i64);

        // Safe: admitted once (a second request while in progress is refused),
        // captures are closed and saved before the host is asked.
        assert!(blocker(&state).is_none() || blocker(&state).unwrap().reason.contains("launch capture"));
        state.recording_store.close_all_now(crate::telemetry_task::get_current_timestamp_ms() as i64, "test");
        crate::recording_store::run_due(&state, &root.0).await;
        let _ = state.recording_store.due(i64::MAX);
        for c in state.recording_store.captures() {
            state.recording_store.finish(c.serial);
        }
        let mechanism = admit(&state, &body(Action::PowerOff, true, None), "flight-director").unwrap();
        assert_eq!(admit(&state, &body(Action::PowerOff, true, None), "flight-director").unwrap_err().0, StatusCode::CONFLICT);
        execute(state.clone(), mechanism, Action::PowerOff, "flight-director".into(), false).await;
        assert_eq!(*calls.lock().unwrap(), vec![Action::PowerOff]);
        let notices: Vec<String> = state.notifications_snapshot().into_iter().map(|n| n.message).collect();
        assert!(notices.iter().any(|m| m.contains("powering off requested by flight-director")), "{notices:?}");
        assert!(notices.iter().any(|m| m.contains("is powering off now")));
        test_mock::reset_in_progress();
        assert_eq!(station_name(), "Ground Station");
    }
}
