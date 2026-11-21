//! The integrated ground test settings (doc sections 7.2 and 7.3):
//! `mock_wet_dress` and `hitl_replay`, admin only, persisted in the GS data
//! directory with the Launch latch (doc section 7.1), so a station restart
//! does not forget a ground-test image seen in a flight mode.
use super::*;
use std::path::Path;

const FILE: &str = "igt_settings.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// The integrated ground test runs the flight fill steps (nitrogen fill
    /// step and the normally-open checks) instead of the test-fire fill.
    #[serde(default)]
    pub mock_wet_dress: bool,
    /// HITL uses the FC's ground-test replay as its flight data source.
    #[serde(default)]
    pub hitl_replay: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    launch_latched: bool,
}

fn read(dir: &Path) -> Saved {
    let path = dir.join(FILE);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("Ignoring unreadable {}: {error}", path.display());
            Saved::default()
        }),
        Err(_) => Saved::default(),
    }
}

fn write(dir: &Path, saved: &Saved) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(FILE);
    let tmp = path.with_extension("pending");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(saved).unwrap_or_default())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)
}

pub(super) fn load(state: &AppState, dir: PathBuf) {
    let saved = read(&dir);
    let mut inner = state.igt.inner.lock().unwrap();
    inner.settings = saved.settings;
    inner.latched |= saved.launch_latched;
    inner.data_dir = Some(dir);
}

fn save(inner: &Inner) -> std::io::Result<()> {
    match &inner.data_dir {
        Some(dir) => write(dir, &Saved { settings: inner.settings, launch_latched: inner.latched }),
        None => Ok(()),
    }
}

/// Saves the settings and the latch; a failure is logged (the latch still
/// holds in memory).
pub(super) fn persist(inner: &Inner) {
    if let Err(error) = save(inner) {
        log::warn!("Integrated ground test settings were not saved: {error}");
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(default)]
    mock_wet_dress: Option<bool>,
    #[serde(default)]
    hitl_replay: Option<bool>,
}

/// Why the settings cannot change now: only while the station is idle (no
/// GSE sequence, cold flow, turbocharge or Launch in progress, the station in
/// Startup or Idle, and no Launch latched or committed).
// Safety catalog (About safety): igt-settings. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn change_block_reason(state: &AppState) -> Option<String> {
    if launched(state) {
        return Some("a Launch is latched or committed; reset the station first".into());
    }
    if let Some(busy) = crate::station_mode_switch::station_busy(state, crate::rrls::is_rrls()) {
        return Some(busy);
    }
    match state.local_flight_state_snapshot() {
        FlightState::Startup | FlightState::Idle => None,
        other => Some(format!("the station is in {other:?}, not Startup or Idle")),
    }
}

/// `PUT /api/igt/settings`: partial, admin only, refused unless idle.
pub(crate) fn update(state: &Arc<AppState>, principal: &AuthPrincipal, body: &[u8]) -> Result<Settings, (StatusCode, String)> {
    if principal.anonymous || !principal.is_admin() {
        return Err(refusal(StatusCode::FORBIDDEN, "Only an administrator may change the ground test settings."));
    }
    if crate::rrls::is_rrls() {
        return Err(refusal(StatusCode::FORBIDDEN, "Ground test settings are changed only on the primary GroundStation."));
    }
    let update: Update = serde_json::from_slice(body).map_err(|_| {
        refusal(StatusCode::BAD_REQUEST, "Expected {mock_wet_dress?: bool, hitl_replay?: bool}.")
    })?;
    let current = state.igt.settings();
    let next = Settings {
        mock_wet_dress: update.mock_wet_dress.unwrap_or(current.mock_wet_dress),
        hitl_replay: update.hitl_replay.unwrap_or(current.hitl_replay),
    };
    if next == current {
        return Ok(current);
    }
    if let Some(reason) = change_block_reason(state) {
        return Err(refusal(StatusCode::CONFLICT, format!("Ground test settings cannot change now: {reason}.")));
    }
    {
        let mut inner = state.igt.inner.lock().unwrap();
        inner.settings = next;
        if let Err(error) = save(&inner) {
            inner.settings = current;
            return Err(refusal(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Ground test settings were not saved: {error}"),
            ));
        }
    }
    let actor = principal.username.clone().unwrap_or_default();
    let text = format!(
        "Ground test settings changed by {actor}: mock wet dress {}, HITL replay {}.",
        if next.mock_wet_dress { "on" } else { "off" },
        if next.hitl_replay { "on" } else { "off" }
    );
    log::info!("{text}");
    crate::web::persist_message_db_only(state, text);
    publish(state);
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    Ok(next)
}

pub(super) async fn get_settings(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(response) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return response;
    }
    ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(state.igt.settings())).into_response()
}

pub(super) async fn put_settings(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    // The idle check may wait out a briefly held GSE lock.
    let result = tokio::task::spawn_blocking(move || update(&state, &principal, &body)).await;
    match result {
        Ok(Ok(settings)) => Json(settings).into_response(),
        Ok(Err(error)) => error_response(error),
        Err(_) => error_response(refusal(StatusCode::INTERNAL_SERVER_ERROR, "Ground test settings update failed.")),
    }
}
