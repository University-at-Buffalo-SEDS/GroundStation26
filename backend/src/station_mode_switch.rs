//! Station mode switch by controlled restart into another installed build.
//!
//! A mode is a compile-time build (normal, HITL, test fire, testing), so this
//! process never changes its own mode. An authorised operator on an idle
//! station selects an installed build; Confirm saves that selection durably,
//! then the backend shuts down through the normal SIGTERM path and exits with
//! [`RESTART_EXIT_CODE`]. The supervisor (`station_modes.py`, used by
//! `run_groundstation.py --switchable-modes` and the systemd units) reads the
//! selection and starts the matching binary. An RRLS switches only itself; the
//! primary's mode is never changed from here.
//!
//! HTTP contract (UI `station_mode_contract.rs`, wire V2):
//! `GET /api/station-modes`, `POST /api/station-modes/prepare`,
//! `POST /api/station-modes/confirm`. Options use `transition_kind: "restart"`
//! and the restart itself is the fence, so `epoch_fencing_supported` stays
//! false and ordinary command admission is never closed by this module.
use crate::{
    auth::{AuthPrincipal, Permission, StreamRole},
    state::AppState,
    station_mode::{Mode, local_mode},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Exit status meaning "restart me into the saved selection".
pub const RESTART_EXIT_CODE: i32 = 75;
pub(crate) const SELECTION_FILE: &str = "selection.json";
const INSTANCE_FILE: &str = "instance-id";
const TICKET_TTL: Duration = Duration::from_millis(15_000);
const VALID_FOR_MS: u64 = 5_000;
/// Gives the Confirm receipt time to reach the client before shutdown starts.
const RESTART_DELAY: Duration = Duration::from_millis(600);
const ALL_MODES: [Mode; 4] = [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing];
/// Operators other than administrators need this command name allowed.
pub(crate) const COMMAND_NAME: &str = "StationMode";

pub(crate) fn mode_id(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "normal",
        Mode::Hitl => "hitl",
        Mode::TestFire => "test_fire",
        Mode::Testing => "testing",
    }
}
fn parse_mode(id: &str) -> Option<Mode> {
    ALL_MODES.into_iter().find(|mode| mode_id(*mode) == id)
}
fn mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "Normal",
        Mode::Hitl => "HITL",
        Mode::TestFire => "Test fire",
        Mode::Testing => "Testing",
    }
}

/// One selectable build plus its simulation flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub mode: Mode,
    pub simulation: bool,
}
impl Target {
    pub(crate) fn id(self) -> String {
        if self.simulation {
            format!("{}-simulation", mode_id(self.mode))
        } else {
            mode_id(self.mode).to_owned()
        }
    }
    fn parse(id: &str) -> Option<Self> {
        let (mode, simulation) = match id.strip_suffix("-simulation") {
            Some(mode) => (mode, true),
            None => (id, false),
        };
        Some(Self {
            mode: parse_mode(mode)?,
            simulation,
        })
    }
    fn label(self, rrls: bool) -> String {
        let mode = mode_label(self.mode);
        match (rrls, self.mode, self.simulation) {
            (true, _, false) => format!("RRLS {mode} · paired primary over radio"),
            (true, _, true) => format!("RRLS {mode} · SIMULATED paired primary"),
            (false, Mode::Testing, true) => "Testing · internal flight simulation".into(),
            (false, Mode::Testing, false) => "Testing · hardware links".into(),
            (false, _, _) => format!("{mode} · live outputs"),
        }
    }
    fn output_domain(self, rrls: bool) -> String {
        match (rrls, self.simulation) {
            (true, false) => {
                "Radio link to the paired primary; live operations stay read-only on the RRLS".into()
            }
            (true, true) => {
                "Simulated paired primary on a local pseudo-terminal; no radio output".into()
            }
            (false, true) => "Simulated rocket telemetry and actions from the testing build's internal flight model".into(),
            (false, false) => format!(
                "Physical station links and outputs as defined by the {} build",
                mode_label(self.mode)
            ),
        }
    }
}
/// The primary's simulation is the testing build's internal flight model
/// (GS_SIM_MODE); the HITL, test-fire and normal builds compile it out. An
/// RRLS can simulate its paired primary in any build.
pub(crate) fn simulation_supported(mode: Mode, rrls: bool) -> bool {
    rrls || mode == Mode::Testing
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub rrls: bool,
    pub supervised: bool,
    pub flight_sim: bool,
    pub dir: PathBuf,
    /// Modes this installation has built binaries for (from the supervisor).
    pub available: Vec<Mode>,
    pub current: Target,
    pub restart_delay: Duration,
}
impl Config {
    fn from_env() -> Self {
        let rrls = crate::rrls::is_rrls();
        let supervised = std::env::var("GS_STATION_MODE_SUPERVISOR").as_deref() == Ok("1");
        let available = std::env::var("GS_STATION_MODE_AVAILABLE")
            .ok()
            .filter(|_| supervised)
            .map(|list| parse_available(&list))
            .unwrap_or_default();
        let mode = local_mode();
        Self {
            rrls,
            supervised,
            flight_sim: crate::flight_sim_runtime::enabled(),
            dir: std::env::var_os("GS_STATION_MODE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data").join("station-mode")),
            available,
            current: Target {
                mode,
                simulation: if rrls {
                    crate::rrls_sim_primary::enabled()
                } else {
                    mode == Mode::Testing && crate::flight_sim::sim_mode_enabled()
                },
            },
            restart_delay: RESTART_DELAY,
        }
    }
    fn role(&self) -> &'static str {
        if self.rrls { "rrls" } else { "primary" }
    }
    fn targets(&self) -> Vec<Target> {
        ALL_MODES
            .into_iter()
            .filter(|mode| self.available.contains(mode))
            .flat_map(|mode| {
                [false, true]
                    .into_iter()
                    .filter(move |simulation| !simulation || simulation_supported(mode, self.rrls))
                    .map(move |simulation| Target { mode, simulation })
            })
            .collect()
    }
    fn change_supported(&self) -> Result<(), String> {
        if self.flight_sim {
            return Err("This is an isolated FlightSim session; it cannot restart into a station build.".into());
        }
        if !self.supervised {
            return Err("This backend was started directly, not by the station mode supervisor, so it cannot restart into another build. Start it with run_groundstation.py --switchable-modes or the station-mode systemd unit.".into());
        }
        if self.targets().len() < 2 {
            return Err("Only one station build is installed. Build the other modes with run_groundstation.py --switchable-modes.".into());
        }
        Ok(())
    }
}
pub(crate) fn parse_available(list: &str) -> Vec<Mode> {
    let mut modes: Vec<Mode> = list
        .split(',')
        .filter_map(|id| parse_mode(id.trim()))
        .collect();
    modes.sort_by_key(|mode| ALL_MODES.iter().position(|m| m == mode));
    modes.dedup();
    modes
}

/// Durable record read by the supervisor at start.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Selection {
    pub version: u8,
    pub role: String,
    pub mode: String,
    pub simulation: bool,
    pub target_id: String,
    pub transition_id: String,
    pub requested_by: String,
    pub requested_at_ms: u64,
}
fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension("pending");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_selection(dir: &Path) -> Option<Selection> {
    let bytes = std::fs::read(dir.join(SELECTION_FILE)).ok()?;
    serde_json::from_slice::<Selection>(&bytes)
        .ok()
        .filter(|s| s.version == 1)
}
fn instance_id(dir: &Path) -> String {
    let path = dir.join(INSTANCE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let text = text.trim();
        if (8..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return text.to_owned();
        }
    }
    let fresh = format!("{:032x}", rand::random::<u128>());
    if let Err(error) = write_durable(&path, fresh.as_bytes()) {
        log::warn!("Station mode instance id is not persistent: {error}");
    }
    fresh
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Transition {
    id: String,
    target_id: String,
    state: &'static str,
    phase: String,
    error: Option<String>,
}
impl Transition {
    fn json(&self) -> Value {
        json!({"id":self.id,"target_id":self.target_id,"state":self.state,"phase":self.phase,"error":self.error})
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    backend_instance: String,
    principal_context: String,
    authority_id: String,
    revision: String,
    policy_epoch: String,
    current_id: String,
    target_id: String,
}
#[derive(Deserialize)]
struct Confirmation {
    #[serde(flatten)]
    binding: Binding,
    ticket: String,
    transition_id: String,
    confirmation: String,
}
struct Ticket {
    ticket: String,
    transition_id: String,
    binding: Binding,
    target: Target,
    principal: String,
    created: Instant,
}
#[derive(Default)]
struct Runtime {
    ticket: Option<Ticket>,
    transition: Option<Transition>,
    restarting: bool,
}

/// Fired once when an accepted switch should shut the process down.
#[derive(Default)]
pub(crate) struct RestartSignal {
    requested: AtomicBool,
    notify: tokio::sync::Notify,
}
impl RestartSignal {
    fn fire(&self) {
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

pub(crate) struct Switch {
    config: Config,
    instance: String,
    epoch: String,
    runtime: Mutex<Runtime>,
    signal: Arc<RestartSignal>,
}
fn restart_signal_global() -> &'static Arc<RestartSignal> {
    static SIGNAL: OnceLock<Arc<RestartSignal>> = OnceLock::new();
    SIGNAL.get_or_init(Default::default)
}
fn global() -> Arc<Switch> {
    static SWITCH: OnceLock<Arc<Switch>> = OnceLock::new();
    SWITCH
        .get_or_init(|| Arc::new(Switch::new(Config::from_env(), restart_signal_global().clone())))
        .clone()
}
/// Resolves once an accepted mode switch requests the clean shutdown.
pub(crate) async fn restart_requested() {
    restart_signal_global().notify.notified().await
}
/// Exit status for `main` after a clean shutdown.
pub(crate) fn exit_code() -> Option<i32> {
    restart_signal_global()
        .requested()
        .then_some(RESTART_EXIT_CODE)
}
/// Log the effective build and any completed switch once at start.
pub(crate) fn start() {
    let switch = global();
    if let Some(transition) = switch.runtime.lock().unwrap().transition.as_ref() {
        log::info!(
            "Station mode transition {} {}: {}",
            transition.id,
            transition.state,
            transition.error.as_deref().unwrap_or(&transition.phase)
        );
    }
}

impl Switch {
    pub(crate) fn new(config: Config, signal: Arc<RestartSignal>) -> Self {
        let instance = instance_id(&config.dir);
        // The saved selection names the transition this process completes.
        let transition = config
            .supervised
            .then(|| read_selection(&config.dir))
            .flatten()
            .filter(|selection| selection.role == config.role())
            .map(|selection| {
                let requested = Target::parse(&selection.target_id);
                if requested == Some(config.current) {
                    Transition {
                        id: selection.transition_id,
                        target_id: selection.target_id,
                        state: "applied",
                        phase: format!("Restarted into {}", config.current.label(config.rrls)),
                        error: None,
                    }
                } else {
                    Transition {
                        id: selection.transition_id,
                        target_id: selection.target_id.clone(),
                        state: "failed",
                        phase: "Supervisor started a different build".into(),
                        error: Some(format!(
                            "Requested {} but this process is {}. Check that the selected build is installed.",
                            selection.target_id,
                            config.current.label(config.rrls)
                        )),
                    }
                }
            });
        Self {
            instance,
            epoch: format!("boot-{:032x}", rand::random::<u128>()),
            runtime: Mutex::new(Runtime {
                transition,
                ..Default::default()
            }),
            config,
            signal,
        }
    }
    fn backend_instance(&self) -> String {
        format!("{}-{}", self.config.role(), self.instance)
    }
    fn authority_id(&self) -> String {
        format!("station-mode-switch/{}", self.config.role())
    }
    fn revision(&self) -> String {
        let available: Vec<_> = self.config.available.iter().map(|m| mode_id(*m)).collect();
        format!("{}:{}", self.epoch, available.join(","))
    }
    fn mode_entry(&self, mode: Mode) -> Value {
        json!({"id": mode_id(mode), "label": mode_label(mode)})
    }
    fn target_json(&self, target: Target, busy: Option<&str>) -> Value {
        let unsupported = self.config.change_supported().err();
        let reason = unsupported.as_deref().or(busy);
        json!({
            "id": target.id(),
            "mode_id": mode_id(target.mode),
            "simulation": target.simulation,
            "label": target.label(self.config.rrls),
            "available": reason.is_none(),
            "reason": reason,
            "transition_kind": "restart",
            "output_domain": target.output_domain(self.config.rrls),
        })
    }
    fn current_json(&self) -> Value {
        let current = self.config.current;
        json!({
            "id": current.id(),
            "mode_id": mode_id(current.mode),
            "label": current.label(self.config.rrls),
            "simulation": current.simulation,
            "output_domain": current.output_domain(self.config.rrls),
        })
    }
    fn binding(&self, principal: &str, target_id: &str) -> Binding {
        Binding {
            backend_instance: self.backend_instance(),
            principal_context: principal.to_owned(),
            authority_id: self.authority_id(),
            revision: self.revision(),
            policy_epoch: self.epoch.clone(),
            current_id: self.config.current.id(),
            target_id: target_id.to_owned(),
        }
    }
    fn snapshot(&self, principal: &AuthPrincipal, busy: Option<String>) -> Value {
        let runtime = self.runtime.lock().unwrap();
        let busy = if runtime.restarting {
            Some("A mode restart is already in progress.".to_owned())
        } else {
            busy
        };
        let mut modes: Vec<Mode> = self.config.available.clone();
        if !modes.contains(&self.config.current.mode) {
            modes.push(self.config.current.mode);
        }
        modes.sort_by_key(|mode| ALL_MODES.iter().position(|m| m == mode));
        let mut notes = Vec::new();
        if let Err(reason) = self.config.change_supported() {
            notes.push(reason);
        } else if let Some(busy) = busy.as_ref() {
            notes.push(format!("Mode changes are refused right now: {busy}"));
        }
        if self.config.rrls {
            notes.push("This switches only this RRLS. The primary's mode is never changed from here; the primary operating mode shown elsewhere still follows the primary.".into());
        }
        json!({
            "version": 2,
            "backend_instance": self.backend_instance(),
            "principal_context": principal_context(principal),
            "authority_id": self.authority_id(),
            "revision": self.revision(),
            "policy_epoch": self.epoch,
            "authority_current": true,
            "valid_for_ms": VALID_FOR_MS,
            "epoch_fencing_supported": false,
            "control_origin_required": false,
            "admissions_open": true,
            "current": self.current_json(),
            "modes": modes.iter().map(|m| self.mode_entry(*m)).collect::<Vec<_>>(),
            "permissions": {"can_change": can_change(principal), "can_cancel_transition": false},
            "change_supported": self.config.change_supported().is_ok(),
            "cancel_supported": false,
            "options": self.config.targets().into_iter().map(|t| self.target_json(t, busy.as_deref())).collect::<Vec<_>>(),
            "transition": runtime.transition.as_ref().map(Transition::json),
            "reason": (!notes.is_empty()).then(|| notes.join(" ")),
        })
    }
    fn phrase(target: Target) -> String {
        format!("RESTART {}", target.id().to_uppercase())
    }
    fn admit(&self, principal: &AuthPrincipal, state: &AppState, binding: &Binding) -> Result<Target, Refusal> {
        if !can_change(principal) {
            return Err(Refusal::forbidden(
                "Station mode changes need an administrator or an operator allowed the StationMode command.",
            ));
        }
        self.config.change_supported().map_err(Refusal::conflict)?;
        if self.runtime.lock().unwrap().restarting {
            return Err(Refusal::conflict("A mode restart is already in progress."));
        }
        let target = Target::parse(&binding.target_id)
            .filter(|target| self.config.targets().contains(target))
            .ok_or_else(|| Refusal::conflict("That mode and simulation combination is not installed on this station."))?;
        if *binding != self.binding(&principal_context(principal), &binding.target_id) {
            return Err(Refusal::conflict(
                "Station mode status changed since it was read. Refresh and review again.",
            ));
        }
        if target == self.config.current {
            return Err(Refusal::conflict("That combination is already active."));
        }
        if let Some(busy) = station_busy(state, self.config.rrls) {
            return Err(Refusal::conflict(format!("Station is not idle: {busy}")));
        }
        Ok(target)
    }
    fn prepare(&self, principal: &AuthPrincipal, state: &AppState, body: &[u8]) -> Result<Value, Refusal> {
        let binding: Binding = serde_json::from_slice(body)
            .map_err(|_| Refusal::bad_request("Invalid station mode review request."))?;
        let target = self.admit(principal, state, &binding)?;
        let ticket = format!("{:032x}", rand::random::<u128>());
        let transition_id = format!("switch-{:032x}", rand::random::<u128>());
        let mut summary = format!(
            "The station saves this selection, shuts down cleanly and its supervisor restarts it with the {} build{}. Live connections drop for a few seconds and reconnect on their own. The switch is allowed only while the station is idle and is checked again just before the restart.",
            mode_label(target.mode),
            if target.simulation { " in simulation" } else { "" },
        );
        if self.config.rrls {
            summary.push_str(" This changes only this RRLS; the primary's mode is never changed from here.");
            if target.simulation {
                summary.push_str(" In simulation the RRLS radio is replaced by a simulated paired primary and sends no radio output.");
            }
        }
        let challenge = json!({
            "backend_instance": binding.backend_instance,
            "principal_context": binding.principal_context,
            "authority_id": binding.authority_id,
            "revision": binding.revision,
            "policy_epoch": binding.policy_epoch,
            "current_id": binding.current_id,
            "target_id": binding.target_id,
            "ticket": ticket,
            "transition_id": transition_id,
            "current": self.current_json(),
            "target": self.target_json(target, None),
            "summary": summary,
            "required_confirmation": Self::phrase(target),
            "expires_in_ms": TICKET_TTL.as_millis() as u64,
        });
        self.runtime.lock().unwrap().ticket = Some(Ticket {
            ticket,
            transition_id,
            binding,
            target,
            principal: principal_context(principal),
            created: Instant::now(),
        });
        Ok(challenge)
    }
    fn confirm(
        self: &Arc<Self>,
        principal: &AuthPrincipal,
        state: &Arc<AppState>,
        body: &[u8],
    ) -> Result<Value, Refusal> {
        let request: Confirmation = serde_json::from_slice(body)
            .map_err(|_| Refusal::bad_request("Invalid station mode confirmation."))?;
        // Single use: any attempt naming this ticket consumes it. A request
        // naming another ticket cannot retire the pending review.
        let ticket = {
            let mut runtime = self.runtime.lock().unwrap();
            let named = runtime
                .ticket
                .as_ref()
                .is_some_and(|t| t.ticket == request.ticket && t.transition_id == request.transition_id);
            named.then(|| runtime.ticket.take()).flatten()
        }
        .ok_or_else(|| Refusal::conflict("This mode review is unknown or was already used. Review again."))?;
        if ticket.created.elapsed() > TICKET_TTL {
            return Err(Refusal::conflict("This mode review expired. Review again."));
        }
        if ticket.principal != principal_context(principal) || ticket.binding != request.binding {
            return Err(Refusal::conflict("This mode review belongs to another session or target."));
        }
        if request.confirmation != Self::phrase(ticket.target) {
            return Err(Refusal::conflict("The confirmation phrase did not match. Review again."));
        }
        let target = self.admit(principal, state, &request.binding)?;
        let selection = Selection {
            version: 1,
            role: self.config.role().into(),
            mode: mode_id(target.mode).into(),
            simulation: target.simulation,
            target_id: target.id(),
            transition_id: ticket.transition_id.clone(),
            requested_by: principal_context(principal),
            requested_at_ms: crate::telemetry_task::get_current_timestamp_ms(),
        };
        let path = self.config.dir.join(SELECTION_FILE);
        let previous = std::fs::read(&path).ok();
        let bytes = serde_json::to_vec_pretty(&selection).map_err(|e| Refusal::internal(e.to_string()))?;
        write_durable(&path, &bytes)
            .map_err(|e| Refusal::internal(format!("Could not save the mode selection: {e}")))?;
        let transition = Transition {
            id: ticket.transition_id.clone(),
            target_id: target.id(),
            state: "pending",
            phase: format!("Restarting into {}", target.label(self.config.rrls)),
            error: None,
        };
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.restarting = true;
            runtime.transition = Some(transition.clone());
        }
        log::warn!(
            "Station mode switch accepted: {} -> {} by {}; restarting",
            self.config.current.id(),
            target.id(),
            selection.requested_by
        );
        let switch = self.clone();
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(switch.config.restart_delay).await;
            switch.finish_restart(&state, &path, previous);
        });
        Ok(json!({
            "version": 2,
            "backend_instance": request.binding.backend_instance,
            "principal_context": request.binding.principal_context,
            "authority_id": request.binding.authority_id,
            "policy_epoch": request.binding.policy_epoch,
            "current_id": request.binding.current_id,
            "target_id": request.binding.target_id,
            "accepted": true,
            "transition": transition.json(),
        }))
    }
    /// Last check before shutdown: a station that became active keeps running
    /// in its current build and the saved selection is restored.
    fn finish_restart(&self, state: &AppState, path: &Path, previous: Option<Vec<u8>>) {
        let busy = station_busy(state, self.config.rrls);
        let mut runtime = self.runtime.lock().unwrap();
        let Some(busy) = busy else {
            drop(runtime);
            self.signal.fire();
            return;
        };
        let restored = match previous {
            Some(bytes) => write_durable(path, &bytes),
            None => std::fs::remove_file(path).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
            }),
        };
        runtime.restarting = false;
        if let Some(transition) = runtime.transition.as_mut() {
            transition.state = "failed";
            transition.phase = "Restart cancelled; the station stays in its current build".into();
            transition.error = Some(match restored {
                Ok(()) => format!("Station became active before the restart: {busy}"),
                Err(error) => format!(
                    "Station became active before the restart ({busy}) and the previous selection could not be restored: {error}. The next restart may use the new selection."
                ),
            });
        }
        log::warn!("Station mode restart cancelled: {busy}");
    }
}

fn principal_context(principal: &AuthPrincipal) -> String {
    match (&principal.username, principal.anonymous) {
        (Some(name), false) => format!("user:{name}"),
        _ => "anonymous".into(),
    }
}
/// Administrator, or a commanding operator explicitly allowed StationMode
/// (an empty command list allows every command, as elsewhere).
pub(crate) fn can_change(principal: &AuthPrincipal) -> bool {
    !principal.anonymous
        && principal.username.is_some()
        && (principal.roles.contains(&StreamRole::Admin)
            || principal.allows_command_name(COMMAND_NAME))
}

/// Why the station is not idle enough for a restart, if anything.
pub(crate) fn station_busy(state: &AppState, rrls: bool) -> Option<String> {
    use crate::types::FlightState;
    if state.shutdown_requested.load(Ordering::SeqCst) {
        return Some("the backend is already shutting down".into());
    }
    if crate::gse::active(state) {
        return Some("a GSE sequence is running".into());
    }
    if crate::gse::turbocharge_active_nowait(state) {
        return Some("turbocharge is running or its state is busy".into());
    }
    if crate::gse::claimed(state) {
        return Some("a sequence or manual valve control holds the outputs".into());
    }
    if state.launch_sequence_command_pending.load(Ordering::SeqCst) {
        return Some("a Launch command is pending".into());
    }
    let flight = state.local_flight_state_snapshot();
    if !matches!(flight, FlightState::Startup | FlightState::Idle) {
        return Some(format!(
            "flight state is {flight:?}; switch only from Startup or Idle"
        ));
    }
    if state.launch_clock_snapshot().kind != crate::telemetry_db::LaunchClockKind::Idle {
        return Some("the T clock is running".into());
    }
    if !outputs_settled(state) {
        return Some(
            "outputs are not settled (an abort, cold flow or queued board command is pending)".into(),
        );
    }
    if !crate::board_recovery::coordinator::manual_utc_idle(state) {
        return Some("a board recovery or reset is in progress".into());
    }
    if rrls && crate::rrls::status().forwarding {
        return Some("this RRLS is relaying recovery traffic for the primary".into());
    }
    None
}
fn outputs_settled(state: &AppState) -> bool {
    if state.topology_router.get().is_some() {
        return crate::gse::operator_reset_host_idle(state);
    }
    // No SDK router: nothing can be queued for the boards.
    crate::gse::verified_recovery_host_idle(state)
        && !crate::gse::cold_flow_active(state)
        && state.manual_abort_intent.pending_epoch().is_none()
        && !state.deferred_abort.manual_pending()
}

struct Refusal(StatusCode, String);
impl Refusal {
    fn forbidden(text: impl Into<String>) -> Self {
        Self(StatusCode::FORBIDDEN, text.into())
    }
    fn conflict(text: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, text.into())
    }
    fn bad_request(text: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, text.into())
    }
    fn internal(text: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, text.into())
    }
}
impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

async fn status(switch: Arc<Switch>, state: Arc<AppState>, headers: HeaderMap) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(error) => return error,
    };
    let busy = station_busy(&state, switch.config.rrls);
    Json(switch.snapshot(&principal, busy)).into_response()
}
async fn prepare(switch: Arc<Switch>, state: Arc<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(error) => return error,
    };
    match switch.prepare(&principal, &state, &body) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}
async fn confirm(switch: Arc<Switch>, state: Arc<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        Ok(principal) => principal,
        Err(error) => return error,
    };
    match switch.confirm(&principal, &state, &body) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}
/// The process-wide switch is created on first use, never by building routes.
pub(crate) fn routes() -> Router<Arc<AppState>> {
    routes_with(None)
}
pub(crate) fn routes_with(fixed: Option<Arc<Switch>>) -> Router<Arc<AppState>> {
    let resolve = move || fixed.clone().unwrap_or_else(global);
    let (a, b, c) = (resolve.clone(), resolve.clone(), resolve);
    Router::new()
        .route(
            "/api/station-modes",
            get(move |State(state): State<Arc<AppState>>, headers: HeaderMap| status(a(), state, headers)),
        )
        .route(
            "/api/station-modes/prepare",
            post(move |State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes| {
                prepare(b(), state, headers, body)
            }),
        )
        .route(
            "/api/station-modes/confirm",
            post(move |State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes| {
                confirm(c(), state, headers, body)
            }),
        )
}

#[cfg(test)]
#[path = "station_mode_switch_tests.rs"]
mod tests;
