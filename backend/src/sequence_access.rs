//! Administrator setting "Launch sequence only": while no sensors or valves
//! are on the bench, the ground automation sequences that cannot be checked
//! are switched off so only the launch sequence, manual controls and comms are
//! exercised.
//!
//! The setting never touches Abort, launch (including its QD/retraction wait
//! and every other launch interlock), resets, recovery or manual commands.
//! A sequence whose result is a launch precondition in the current station
//! mode stays enabled: in Normal, Testing and TestFire the launch gate needs
//! the self-test, nitrogen-test and fill qualification, so only HITL (whose
//! launch needs none of them) disables those three.
use crate::{sequences::ActionPolicyMsg, state::AppState, types::TelemetryCommand};
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

/// Inline reason on every control this setting disables.
pub const DISABLED_REASON: &str = "Disabled in admin settings: launch sequence only";
/// Presets the setting when no administrator choice has been saved yet.
pub const PRESET_ENV: &str = "GS_LAUNCH_SEQUENCE_ONLY";
const FILE_NAME: &str = "sequence_access.json";

#[derive(Default)]
pub struct SequenceAccess {
    launch_only: AtomicBool,
    /// Serializes administrator writes and their file update.
    write: std::sync::Mutex<()>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Saved {
    launch_sequence_only: bool,
    #[serde(default)]
    updated_by: Option<String>,
    #[serde(default)]
    updated_at_ms: Option<i64>,
}

/// One sequence the setting can disable: its command name and operator label.
struct Sequence {
    command: &'static str,
    label: &'static str,
    /// Its result is a launch precondition outside HITL, so it stays enabled there.
    launch_qualification: bool,
}
const SEQUENCES: [Sequence; 6] = [
    Sequence { command: "ValveSelfTest", label: "Valve self-test", launch_qualification: true },
    Sequence { command: "NitrogenTest", label: "Nitrogen test", launch_qualification: true },
    Sequence { command: "StartFill", label: "Fill (with its mass trim and resume)", launch_qualification: true },
    Sequence { command: "Turbocharge", label: "Turbocharge", launch_qualification: false },
    Sequence { command: "AutoColdFlow", label: "Auto Cold Flow", launch_qualification: false },
    Sequence { command: "ContinueFillSequence", label: "Guided fill sequence Continue", launch_qualification: false },
];
/// What the setting never disables, for the administrator page.
const ALWAYS_KEPT: [&str; 7] = [
    "Launch sequence (Launch, GroundStation Launch, igniter sequence)",
    "QD / fill-plumbing retraction wait and every other launch interlock",
    "Abort and every abort path",
    "Reset and FC reset to Startup",
    "Recovery (pyro Deploy/Expand)",
    "Manual valve, actuator and other manual commands",
    "Comms, telemetry and RRLS",
];

fn disables(sequence: &Sequence) -> bool {
    crate::runtime_mode::is_hitl() || !sequence.launch_qualification
}

pub fn launch_only(state: &AppState) -> bool {
    state.sequence_access.launch_only.load(Ordering::Acquire)
}

/// Whether the setting disables the named sequence command in the current mode.
pub fn disabled_command(state: &AppState, name: &str) -> bool {
    launch_only(state)
        && SEQUENCES
            .iter()
            .any(|sequence| sequence.command == name && disables(sequence))
}

/// The inline reason `cmd` is refused by this setting, if it is.
pub fn refusal(state: &AppState, cmd: &TelemetryCommand) -> Option<&'static str> {
    disabled_command(state, crate::sequences::command_name(cmd)).then_some(DISABLED_REASON)
}

/// Disable the affected controls with the inline reason. Presentation of the
/// same rule that `refusal` enforces at admission.
pub fn project_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    if !launch_only(state) {
        return;
    }
    for control in &mut policy.controls {
        if disabled_command(state, &control.cmd) {
            control.enabled = false;
            control.disabled_reason = Some(DISABLED_REASON.into());
            control.blink = crate::sequences::BlinkMode::None;
        }
    }
}

fn path(state: &AppState) -> PathBuf {
    Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE_NAME)
}

fn env_preset() -> bool {
    std::env::var(PRESET_ENV).is_ok_and(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

/// Load the saved choice at boot. Without one, `GS_LAUNCH_SEQUENCE_ONLY`
/// presets it; otherwise it starts off. A saved file always wins.
pub fn restore(state: &AppState) {
    let enabled = match std::fs::read(path(state)) {
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) => saved.launch_sequence_only,
            Err(error) => {
                log::warn!("Ignoring unreadable {FILE_NAME}: {error}");
                env_preset()
            }
        },
        Err(_) => env_preset(),
    };
    if enabled {
        log::info!("Launch sequence only is enabled: non-launch GSE sequences are disabled");
    }
    state.sequence_access.launch_only.store(enabled, Ordering::Release);
}

fn persist(state: &AppState, saved: &Saved) -> Result<(), String> {
    let path = path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("upload");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}

/// Why enabling must be refused now: a sequence this setting would disable
/// is running or paused. A running sequence is never stopped silently.
fn enable_block_reason(state: &AppState) -> Option<String> {
    crate::gse::running_disabled_sequence(state, crate::runtime_mode::is_hitl()).map(|name| {
        format!(
            "{name} is running or paused. Let it finish or cancel it before enabling launch sequence only."
        )
    })
}

/// Change the setting. Enabling is checked and applied under the GSE command
/// gate, so no disabled sequence can start between the check and the change.
pub(crate) fn set(state: &AppState, enabled: bool, actor: Option<String>) -> Result<(), (StatusCode, String)> {
    if crate::rrls::is_rrls() {
        return Err((
            StatusCode::FORBIDDEN,
            "Change launch sequence only on the primary GroundStation.".into(),
        ));
    }
    let _write = state.sequence_access.write.lock().unwrap();
    crate::gse::with_command_gate(|| {
        if enabled && !launch_only(state) && let Some(reason) = enable_block_reason(state) {
            return Err((StatusCode::CONFLICT, reason));
        }
        let saved = Saved {
            launch_sequence_only: enabled,
            updated_by: actor.clone(),
            updated_at_ms: Some(crate::telemetry_task::get_current_timestamp_ms() as i64),
        };
        persist(state, &saved).map_err(|error| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Cannot save the setting: {error}"))
        })?;
        state.sequence_access.launch_only.store(enabled, Ordering::Release);
        Ok(())
    })?;
    log::info!(
        "Launch sequence only {} by administrator {}",
        if enabled { "enabled" } else { "disabled" },
        actor.as_deref().unwrap_or("unknown")
    );
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Status {
    pub launch_sequence_only: bool,
    /// Why it cannot be turned on right now; None when it can.
    pub enable_blocked_reason: Option<String>,
    /// Sequences disabled while it is on, in the current station mode.
    pub disabled_sequences: Vec<String>,
    /// Sequences that stay enabled in this mode because launch needs their result.
    pub kept_sequences: Vec<String>,
    pub always_kept: Vec<String>,
    pub can_change: bool,
    pub preset_env: String,
}
pub(crate) fn status(state: &AppState) -> Status {
    let (disabled, kept): (Vec<_>, Vec<_>) = SEQUENCES.iter().partition(|s| disables(s));
    let rrls = crate::rrls::is_rrls();
    Status {
        launch_sequence_only: launch_only(state),
        enable_blocked_reason: if rrls {
            Some("Change launch sequence only on the primary GroundStation.".into())
        } else if launch_only(state) {
            None
        } else {
            enable_block_reason(state)
        },
        disabled_sequences: disabled.iter().map(|s| s.label.to_string()).collect(),
        kept_sequences: kept
            .iter()
            .map(|s| format!("{} (launch qualification needs it in this mode)", s.label))
            .collect(),
        always_kept: ALWAYS_KEPT.iter().map(|s| s.to_string()).collect(),
        can_change: !rrls,
        preset_env: PRESET_ENV.into(),
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/api/admin/sequence-access", get(get_status).post(post_status))
}
async fn get_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::admin::administrator(&state, &headers).await {
        return error;
    }
    Json(status(&state)).into_response()
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Update {
    pub launch_sequence_only: bool,
}
async fn post_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<Update>,
) -> Response {
    let principal = match crate::admin::administrator(&state, &headers).await {
        Ok(principal) => principal,
        Err(error) => return error,
    };
    match set(&state, update.launch_sequence_only, principal.username.clone()) {
        Ok(()) => {
            crate::sequences::refresh_action_policy_projection(&state);
            state.broadcast_action_policy_snapshot();
            Json(status(&state)).into_response()
        }
        Err(error) => error.into_response(),
    }
}
