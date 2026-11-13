//! Administrator setting "Physical panel": whether the five dual-meaning
//! panel buttons work as individual valve controls or as sequence controls.
//!
//! The meaning was, and still is by default, the GSE setting `grouped_panel`
//! (`gse_sequence.json`): true means sequence control. An administrator can
//! override it here; the override is saved in the data directory. With no
//! saved choice the station does exactly what it did before.
//!
//! Only the button-to-command translation (`gse::panel_name`) changes. A press
//! still passes the same gates as the on-screen button of the command it
//! sends (action policy, command policy, launch-sequence-only, interlocks),
//! and the LEDs follow the same translation. Abort, Launch, Launch arm,
//! All-buttons enable, Warning ack, Igniter and QD are not affected.
use crate::{state::AppState, types::FlightState};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FILE_NAME: &str = "physical_panel.json";
const RRLS_REASON: &str = "Change the physical panel on the primary GroundStation.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelMode {
    /// Each button drives its own valve.
    Valves,
    /// The buttons run the GSE sequences (the "grouped" panel).
    Sequences,
}
impl PanelMode {
    fn from_grouped(grouped: bool) -> Self {
        if grouped { PanelMode::Sequences } else { PanelMode::Valves }
    }
    pub fn label(self) -> &'static str {
        match self {
            PanelMode::Valves => "Individual valve control",
            PanelMode::Sequences => "Sequence control",
        }
    }
}

/// One dual-meaning button: physical label, BCM pin, valve and sequence command.
pub struct DualButton {
    pub physical: &'static str,
    pub pin: u8,
    pub valve: &'static str,
    pub valve_label: &'static str,
    pub sequence: &'static str,
    pub sequence_label: &'static str,
}
pub const DUAL_BUTTONS: [DualButton; 5] = [
    DualButton { physical: "Pilot", pin: crate::gpio_panel::PILOT_VALVE_PIN, valve: "Pilot", valve_label: "Pilot valve", sequence: "ValveSelfTest", sequence_label: "Valve self-test" },
    DualButton { physical: "Vent", pin: crate::gpio_panel::NORMALLY_OPEN_PIN, valve: "NormallyOpen", valve_label: "Vent (normally open) valve", sequence: "PauseFill", sequence_label: "Pause fill" },
    DualButton { physical: "Dump", pin: crate::gpio_panel::DUMP_PIN, valve: "Dump", valve_label: "Dump valve", sequence: "CancelFill", sequence_label: "Cancel fill" },
    DualButton { physical: "Nitrogen", pin: crate::gpio_panel::NITROGEN_TANK_VALVE_PIN, valve: "Nitrogen", valve_label: "Nitrogen valve", sequence: "NitrogenTest", sequence_label: "Nitrogen test" },
    DualButton { physical: "Nitrous", pin: crate::gpio_panel::NITROUS_TANK_VALVE_PIN, valve: "Nitrous", valve_label: "Nitrous valve", sequence: "StartFill", sequence_label: "Start fill (with recording)" },
];
/// Panel inputs whose meaning never changes.
pub const FIXED_INPUTS: [(&str, u8, &str); 7] = [
    ("Abort", crate::gpio_panel::ABORT_PIN, "Abort (supervised hardware input)"),
    ("Launch", crate::gpio_panel::LAUNCH_PIN, "Launch"),
    ("Igniter", crate::gpio_panel::IGNITER_PIN, "Igniter"),
    ("QD", crate::gpio_panel::RETRACT_PIN, "QD retract"),
    ("Launch arm", crate::gpio_panel::LAUNCH_ARM_PIN, "Launch arm switch"),
    ("All-buttons enable", crate::gpio_panel::ALL_BUTTONS_ENABLE_PIN, "All-buttons enable switch"),
    ("Warning ack", crate::gpio_panel::WARNING_ACK_PIN, "Acknowledge warnings"),
];

/// The physical-button translation for a mode.
pub fn translate(mode: PanelMode, name: &str) -> &str {
    if mode == PanelMode::Valves {
        return name;
    }
    DUAL_BUTTONS
        .iter()
        .find(|button| button.valve == name)
        .map_or(name, |button| button.sequence)
}

#[derive(Default)]
pub struct PanelControl {
    /// Administrator override; None follows the GSE setting.
    choice: Mutex<Option<PanelMode>>,
    updated: Mutex<(Option<String>, Option<i64>)>,
    /// Serializes administrator writes and their file update.
    write: Mutex<()>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Saved {
    panel_mode: Option<PanelMode>,
    #[serde(default)]
    updated_by: Option<String>,
    #[serde(default)]
    updated_at_ms: Option<i64>,
}

pub fn choice(state: &AppState) -> Option<PanelMode> {
    *state.panel_control.choice.lock().unwrap()
}

/// The mode in effect, given the GSE setting's `grouped_panel`.
pub fn effective(state: &AppState, grouped_panel: bool) -> PanelMode {
    choice(state).unwrap_or(PanelMode::from_grouped(grouped_panel))
}

fn station_default(state: &AppState) -> PanelMode {
    PanelMode::from_grouped(state.gse.lock().unwrap().engine.config.grouped_panel)
}

fn path(state: &AppState) -> PathBuf {
    Path::new(&state.placeholder_db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE_NAME)
}

/// Load the saved choice at boot. Without one the GSE setting decides.
pub fn restore(state: &AppState) {
    let saved = match std::fs::read(path(state)) {
        Ok(bytes) => match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) => saved,
            Err(error) => {
                log::warn!("Ignoring unreadable {FILE_NAME}: {error}");
                return;
            }
        },
        Err(_) => return,
    };
    if let Some(mode) = saved.panel_mode {
        log::info!("Physical panel buttons set by administrator: {}", mode.label());
    }
    *state.panel_control.updated.lock().unwrap() = (saved.updated_by, saved.updated_at_ms);
    *state.panel_control.choice.lock().unwrap() = saved.panel_mode;
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

/// Flight states in which the meaning of the buttons must not change.
fn armed_or_flying(flight: FlightState) -> bool {
    matches!(
        flight,
        FlightState::Armed
            | FlightState::Launch
            | FlightState::Ascent
            | FlightState::Coast
            | FlightState::Apogee
            | FlightState::Descent
            | FlightState::Reefing
            | FlightState::InFlight
    )
}

/// Why the panel meaning cannot change now, if it cannot.
pub fn change_block_reason(state: &AppState) -> Option<String> {
    if crate::rrls::is_rrls() {
        return Some(RRLS_REASON.into());
    }
    let flight = state.local_flight_state_snapshot();
    if armed_or_flying(flight) {
        return Some(format!(
            "The station is {flight:?}. Change the panel buttons only before arming or after the flight."
        ));
    }
    if let Some(name) = crate::gse::running_disabled_sequence(state, true) {
        return Some(format!(
            "{name} is running or paused. Let it finish or cancel it before changing the panel buttons."
        ));
    }
    if state.launch_mode_gate.launch_hold().is_some()
        || matches!(
            state.launch_clock_snapshot().kind,
            crate::telemetry_db::LaunchClockKind::TMinus
        )
    {
        return Some("A launch is in progress. Change the panel buttons only while the station is idle.".into());
    }
    None
}

/// Change the setting. Checked and applied under the GSE command gate, so no
/// sequence can start between the check and the change.
pub(crate) fn set(
    state: &AppState,
    mode: Option<PanelMode>,
    actor: Option<String>,
) -> Result<(), (StatusCode, String)> {
    if crate::rrls::is_rrls() {
        return Err((StatusCode::FORBIDDEN, RRLS_REASON.into()));
    }
    let _write = state.panel_control.write.lock().unwrap();
    crate::gse::with_command_gate(|| {
        if let Some(reason) = change_block_reason(state) {
            return Err((StatusCode::CONFLICT, reason));
        }
        let saved = Saved {
            panel_mode: mode,
            updated_by: actor.clone(),
            updated_at_ms: Some(crate::telemetry_task::get_current_timestamp_ms() as i64),
        };
        persist(state, &saved).map_err(|error| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Cannot save the setting: {error}"))
        })?;
        *state.panel_control.updated.lock().unwrap() = (saved.updated_by, saved.updated_at_ms);
        *state.panel_control.choice.lock().unwrap() = mode;
        Ok(())
    })?;
    log::info!(
        "Physical panel buttons set to {} by administrator {}",
        mode.map_or("the station default", PanelMode::label),
        actor.as_deref().unwrap_or("unknown")
    );
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct ButtonRow {
    pub physical: String,
    pub pin: u8,
    pub valve_command: String,
    pub valve_label: String,
    pub sequence_command: String,
    pub sequence_label: String,
    /// The command a press sends now.
    pub current_command: String,
    /// Why the current command is refused by "Launch sequence only".
    pub disabled_reason: Option<String>,
}
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct FixedRow {
    pub physical: String,
    pub pin: u8,
    pub function: String,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Status {
    /// The administrator's choice; None follows the station default.
    pub panel_mode: Option<PanelMode>,
    pub effective_mode: PanelMode,
    /// What the GSE setting `grouped_panel` selects.
    pub station_default: PanelMode,
    pub buttons: Vec<ButtonRow>,
    pub fixed: Vec<FixedRow>,
    /// Shown when sequence control and "Launch sequence only" are both on.
    pub launch_only_note: Option<String>,
    pub change_blocked_reason: Option<String>,
    pub can_change: bool,
    pub updated_by: Option<String>,
    pub updated_at_ms: Option<i64>,
}

pub(crate) fn status(state: &AppState) -> Status {
    let station_default = station_default(state);
    let effective_mode = choice(state).unwrap_or(station_default);
    let buttons: Vec<ButtonRow> = DUAL_BUTTONS
        .iter()
        .map(|button| {
            let current = translate(effective_mode, button.valve);
            ButtonRow {
                physical: button.physical.into(),
                pin: button.pin,
                valve_command: button.valve.into(),
                valve_label: button.valve_label.into(),
                sequence_command: button.sequence.into(),
                sequence_label: button.sequence_label.into(),
                current_command: current.into(),
                disabled_reason: crate::sequence_access::disabled_command(state, current)
                    .then(|| crate::sequence_access::DISABLED_REASON.to_string()),
            }
        })
        .collect();
    let launch_only_note = (effective_mode == PanelMode::Sequences
        && crate::sequence_access::launch_only(state))
    .then(|| {
        let refused: Vec<&str> = DUAL_BUTTONS
            .iter()
            .filter(|b| crate::sequence_access::disabled_command(state, b.sequence))
            .map(|b| b.physical)
            .collect();
        if refused.is_empty() {
            "Launch sequence only is also on. In this station mode it keeps every panel sequence the launch needs, so the panel buttons are unaffected.".to_string()
        } else {
            format!(
                "Launch sequence only is also on: the {} button{} refuse their sequence (\"{}\") and their LEDs show disabled. They do not fall back to valve control.",
                refused.join(", "),
                if refused.len() == 1 { "" } else { "s" },
                crate::sequence_access::DISABLED_REASON
            )
        }
    });
    let (updated_by, updated_at_ms) = state.panel_control.updated.lock().unwrap().clone();
    Status {
        panel_mode: choice(state),
        effective_mode,
        station_default,
        buttons,
        fixed: FIXED_INPUTS
            .iter()
            .map(|(physical, pin, function)| FixedRow {
                physical: (*physical).into(),
                pin: *pin,
                function: (*function).into(),
            })
            .collect(),
        launch_only_note,
        change_blocked_reason: change_block_reason(state),
        can_change: !crate::rrls::is_rrls(),
        updated_by,
        updated_at_ms,
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct PressInput {
    pub physical: String,
    pub pin: u8,
    pub pressed: Option<bool>,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct PressTest {
    pub gpio_available: bool,
    pub message: Option<String>,
    pub inputs: Vec<PressInput>,
}

/// Live level of each panel input (all are active-high), so an operator can
/// find a button on the panel by pressing it.
pub(crate) fn press_test(gpio: &crate::gpio::GpioPins, available: bool) -> PressTest {
    if !available {
        return PressTest {
            gpio_available: false,
            message: Some(if crate::rrls::is_rrls() {
                "The control panel is on the primary GroundStation.".into()
            } else {
                "No GPIO on this host.".into()
            }),
            inputs: Vec::new(),
        };
    }
    let inputs = DUAL_BUTTONS
        .iter()
        .map(|b| (b.physical, b.pin))
        .chain(FIXED_INPUTS.iter().map(|(physical, pin, _)| (*physical, *pin)))
        .map(|(physical, pin)| PressInput {
            physical: physical.into(),
            pin,
            pressed: gpio.read_input_pin(pin).ok(),
        })
        .collect();
    PressTest { gpio_available: true, message: None, inputs }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/admin/physical-panel", get(get_status).post(post_status))
        .route("/api/admin/physical-panel/press-test", get(get_press_test))
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
    /// None returns to the station default.
    pub panel_mode: Option<PanelMode>,
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
    match set(&state, update.panel_mode, principal.username.clone()) {
        Ok(()) => {
            crate::sequences::refresh_action_policy_projection(&state);
            state.broadcast_action_policy_snapshot();
            Json(status(&state)).into_response()
        }
        Err(error) => error.into_response(),
    }
}
async fn get_press_test(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(error) = crate::admin::administrator(&state, &headers).await {
        return error;
    }
    let available = cfg!(feature = "raspberry_pi") && !crate::rrls::is_rrls();
    Json(press_test(&state.gpio, available)).into_response()
}

#[cfg(test)]
#[path = "panel_control_tests.rs"]
mod tests;
