//! Station mode switch.
//!
//! One backend binary holds every mode (normal, HITL, test fire, testing; see
//! `runtime_mode`). An authorised operator on an idle station selects a mode
//! and Simulation flag:
//!
//! - Same Simulation flag: the switch happens inside this process. Confirm
//!   saves the selection durably (the next start uses it), then
//!   `runtime_mode::switch_to` re-checks that the station is idle while every
//!   mode-dependent admission is held off, publishes the new mode with a new
//!   epoch, resets the mode-dependent state ([`apply_switch`]) and announces it
//!   on the websocket (`StationModeChanged`, then a fresh `ActionPolicy`).
//!   Requests admitted under the old mode carry the old epoch and are refused.
//! - Simulation changes: Simulation is a start-up property, so the backend
//!   shuts down through the normal SIGTERM path and exits with
//!   [`RESTART_EXIT_CODE`]; the supervisor (`station_modes.py`, used by
//!   `run_groundstation.py --switchable-modes` and the systemd units) restarts
//!   it into the saved selection.
//!
//! An RRLS switches only itself; the primary's mode is never changed from here.
//!
//! HTTP contract (UI `station_mode_contract.rs`, wire V2):
//! `GET /api/station-modes`, `POST /api/station-modes/prepare`,
//! `POST /api/station-modes/confirm`. Options use `transition_kind: "restart"`
//! (server-fenced: the backend checks and fences the switch itself) and carry
//! `in_process: true` when no restart is needed. `epoch_fencing_supported`
//! stays false: the client never needs the per-request epoch protocol.
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
/// Gives the Confirm receipt time to reach the client before the mode changes.
const SWITCH_DELAY: Duration = Duration::from_millis(200);
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

/// One selectable mode plus its simulation flag.
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
            (false, true) => "Simulated rocket telemetry and actions from testing mode's internal flight model".into(),
            (false, false) => format!(
                "Physical station links and outputs as defined by {} mode",
                mode_label(self.mode)
            ),
        }
    }
}
/// The primary's simulation is testing mode's internal flight model
/// (GS_SIM_MODE, fixed at start). An RRLS can simulate its paired primary in
/// any mode.
pub(crate) fn simulation_supported(mode: Mode, rrls: bool) -> bool {
    rrls || mode == Mode::Testing
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub rrls: bool,
    pub supervised: bool,
    pub flight_sim: bool,
    pub dir: PathBuf,
    /// The Simulation flag this process started with. It never changes in
    /// process; the mode itself is `runtime_mode::current()`.
    pub simulation: bool,
    /// Testing mode's internal flight model is running (fixed at start).
    /// Several checks trust it alone, so leaving testing restarts.
    pub flight_model: bool,
    pub restart_delay: Duration,
    pub switch_delay: Duration,
    /// Station resets that touch files and the boards (GSE qualification
    /// restore, load-cell calibration reload and its DAQ publication). Route
    /// tests turn them off and keep the in-memory resets.
    pub station_resets: bool,
    /// DummyComms stand-ins alive now (`comms::dummy_links`).
    pub stand_in_links: fn() -> usize,
}
impl Config {
    fn from_env() -> Self {
        let rrls = crate::rrls::is_rrls();
        let supervised = std::env::var("GS_STATION_MODE_SUPERVISOR").as_deref() == Ok("1");
        Self {
            rrls,
            supervised,
            flight_sim: crate::flight_sim_runtime::enabled(),
            dir: std::env::var_os("GS_STATION_MODE_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data").join("station-mode")),
            // sim_mode_enabled is true only in a process that started in
            // testing mode with the internal flight model.
            simulation: if rrls {
                crate::rrls_sim_primary::enabled()
            } else {
                crate::flight_sim::sim_mode_enabled()
            },
            flight_model: crate::flight_sim::sim_mode_enabled(),
            restart_delay: RESTART_DELAY,
            switch_delay: SWITCH_DELAY,
            station_resets: true,
            stand_in_links: crate::comms::dummy_links,
        }
    }
    fn role(&self) -> &'static str {
        if self.rrls { "rrls" } else { "primary" }
    }
    fn current(&self) -> Target {
        Target {
            mode: local_mode(),
            simulation: self.simulation,
        }
    }
    fn targets(&self) -> Vec<Target> {
        ALL_MODES
            .into_iter()
            .flat_map(|mode| {
                [false, true]
                    .into_iter()
                    .filter(move |simulation| !simulation || simulation_supported(mode, self.rrls))
                    .map(move |simulation| Target { mode, simulation })
            })
            .collect()
    }
    /// The switch keeps the Simulation flag, so it happens in this process.
    fn in_process(&self, target: Target) -> bool {
        target.simulation == self.simulation && (!self.flight_model || target.mode == Mode::Testing)
    }
    /// Why no switch at all is possible here.
    fn change_supported(&self) -> Result<(), String> {
        if self.flight_sim {
            return Err("This is an isolated FlightSim session; it cannot change station mode.".into());
        }
        Ok(())
    }
    /// Why this particular target cannot be selected, apart from busy checks.
    fn target_supported(&self, target: Target) -> Result<(), String> {
        self.change_supported()?;
        if !self.in_process(target) && !self.supervised {
            return Err("Changing Simulation restarts the backend, and this backend was started directly, not by the station mode supervisor. Start it with run_groundstation.py --switchable-modes or the station-mode systemd unit.".into());
        }
        if self.in_process(target) && target.mode == Mode::Normal && !self.rrls {
            normal_links_present((self.stand_in_links)())?;
        }
        Ok(())
    }
}

/// Normal mode refuses to start without its configured links (it never used
/// stand-ins), so it is never entered while a link is a DummyComms stand-in.
fn normal_links_present(stand_ins: usize) -> Result<(), String> {
    if stand_ins == 0 {
        Ok(())
    } else {
        Err(format!(
            "Normal mode needs every configured link, but {stand_ins} link(s) failed to open at start and use a stand-in. Fix the link and restart the station."
        ))
    }
}

/// Bumped by every in-process switch so connected UIs refetch /api/layout.
static LAYOUT_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) fn layout_revision() -> u64 {
    LAYOUT_REVISION.load(Ordering::Acquire)
}
/// The websocket announcement of the last in-process switch.
#[cfg(not(test))]
static LAST_CHANGE: Mutex<Option<Value>> = Mutex::new(None);
#[cfg(test)]
thread_local! {
    // Tests switch on their own threads (see runtime_mode::test_support).
    static LAST_CHANGE: std::cell::RefCell<Option<Value>> = const { std::cell::RefCell::new(None) };
}
/// `{"mode","label","policy_epoch","layout_revision"}` of the last switch.
pub(crate) fn last_change() -> Option<Value> {
    #[cfg(not(test))]
    return LAST_CHANGE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    #[cfg(test)]
    return LAST_CHANGE.with(|change| change.borrow().clone());
}
fn set_last_change(change: Value) {
    #[cfg(not(test))]
    {
        *LAST_CHANGE.lock().unwrap_or_else(|e| e.into_inner()) = Some(change);
    }
    #[cfg(test)]
    LAST_CHANGE.with(|slot| *slot.borrow_mut() = Some(change));
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
/// Directory holding `selection.json`.
#[cfg(not(test))]
pub(crate) fn selection_dir() -> PathBuf {
    std::env::var_os("GS_STATION_MODE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data").join("station-mode"))
}
/// Mode saved by the last confirmed switch for this station's role.
#[cfg(not(test))]
pub(crate) fn saved_mode() -> Option<Mode> {
    let role = if crate::rrls::is_rrls() { "rrls" } else { "primary" };
    read_selection(&selection_dir())
        .filter(|selection| selection.role == role)
        .and_then(|selection| parse_mode(&selection.mode))
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
                if requested == Some(config.current()) {
                    Transition {
                        id: selection.transition_id,
                        target_id: selection.target_id,
                        state: "applied",
                        phase: format!("Started in {}", config.current().label(config.rrls)),
                        error: None,
                    }
                } else {
                    Transition {
                        id: selection.transition_id,
                        target_id: selection.target_id.clone(),
                        state: "failed",
                        phase: "Supervisor started a different mode".into(),
                        error: Some(format!(
                            "Requested {} but this process is {}. Check the station mode supervisor configuration.",
                            selection.target_id,
                            config.current().label(config.rrls)
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
    /// Changes with every in-process switch: bindings taken before a switch
    /// no longer match.
    fn policy_epoch(&self) -> String {
        format!("{}:epoch-{}", self.epoch, crate::runtime_mode::epoch())
    }
    fn revision(&self) -> String {
        self.policy_epoch()
    }
    fn mode_entry(&self, mode: Mode) -> Value {
        json!({"id": mode_id(mode), "label": mode_label(mode)})
    }
    fn target_json(&self, target: Target, busy: Option<&str>) -> Value {
        let unsupported = self.config.target_supported(target).err();
        let reason = unsupported.as_deref().or(busy);
        json!({
            "id": target.id(),
            "mode_id": mode_id(target.mode),
            "simulation": target.simulation,
            "label": target.label(self.config.rrls),
            "available": reason.is_none(),
            "reason": reason,
            "transition_kind": "restart",
            "in_process": self.config.in_process(target),
            "output_domain": target.output_domain(self.config.rrls),
        })
    }
    fn current_json(&self) -> Value {
        let current = self.config.current();
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
            policy_epoch: self.policy_epoch(),
            current_id: self.config.current().id(),
            target_id: target_id.to_owned(),
        }
    }
    fn snapshot(&self, principal: &AuthPrincipal, busy: Option<String>) -> Value {
        let runtime = self.runtime.lock().unwrap();
        let busy = if runtime.restarting {
            Some("A mode change is already in progress.".to_owned())
        } else {
            busy
        };
        let modes = ALL_MODES;
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
            "policy_epoch": self.policy_epoch(),
            "authority_current": true,
            "valid_for_ms": VALID_FOR_MS,
            "epoch_fencing_supported": false,
            "control_origin_required": false,
            "admissions_open": !runtime.restarting,
            "layout_revision": layout_revision(),
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
    fn phrase(&self, target: Target) -> String {
        if self.config.in_process(target) {
            format!("SWITCH TO {}", target.id().to_uppercase())
        } else {
            format!("RESTART {}", target.id().to_uppercase())
        }
    }
    fn admit(&self, principal: &AuthPrincipal, state: &AppState, binding: &Binding) -> Result<Target, Refusal> {
        if !can_change(principal) {
            return Err(Refusal::forbidden(
                "Station mode changes need an administrator or an operator allowed the StationMode command.",
            ));
        }
        self.config.change_supported().map_err(Refusal::conflict)?;
        if self.runtime.lock().unwrap().restarting {
            return Err(Refusal::conflict("A mode change is already in progress."));
        }
        let target = Target::parse(&binding.target_id)
            .filter(|target| self.config.targets().contains(target))
            .ok_or_else(|| Refusal::conflict("That mode and simulation combination is not available on this station."))?;
        if *binding != self.binding(&principal_context(principal), &binding.target_id) {
            return Err(Refusal::conflict(
                "Station mode status changed since it was read. Refresh and review again.",
            ));
        }
        if target == self.config.current() {
            return Err(Refusal::conflict("That combination is already active."));
        }
        self.config.target_supported(target).map_err(Refusal::conflict)?;
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
        let mut summary = if self.config.in_process(target) {
            format!(
                "The station saves this selection and switches to {} mode in place. Sign-ins and connections are kept; the layout and controls refresh on their own. The switch is allowed only while the station is idle and is checked again at the moment it happens.",
                mode_label(target.mode),
            )
        } else {
            format!(
                "Simulation is set at start, so the station saves this selection, shuts down cleanly and its supervisor restarts it in {} mode{}. Live connections drop for a few seconds and reconnect on their own. The switch is allowed only while the station is idle and is checked again just before the restart.",
                mode_label(target.mode),
                if target.simulation { " in simulation" } else { "" },
            )
        };
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
            "required_confirmation": self.phrase(target),
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
        if request.confirmation != self.phrase(ticket.target) {
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
        let in_process = self.config.in_process(target);
        let transition = Transition {
            id: ticket.transition_id.clone(),
            target_id: target.id(),
            state: "pending",
            phase: if in_process {
                format!("Switching to {}", target.label(self.config.rrls))
            } else {
                format!("Restarting into {}", target.label(self.config.rrls))
            },
            error: None,
        };
        {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.restarting = true;
            runtime.transition = Some(transition.clone());
        }
        log::warn!(
            "Station mode switch accepted: {} -> {} by {}; {}",
            self.config.current().id(),
            target.id(),
            selection.requested_by,
            if in_process { "switching in process" } else { "restarting" }
        );
        let switch = self.clone();
        let state = state.clone();
        tokio::spawn(async move {
            if in_process {
                tokio::time::sleep(switch.config.switch_delay).await;
                switch.finish_switch(&state, target, &path, previous);
            } else {
                tokio::time::sleep(switch.config.restart_delay).await;
                switch.finish_restart(&state, &path, previous);
            }
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
    /// Changes the mode in this process. The idle check runs again under the
    /// runtime-mode gate, so no mode-dependent operation can start between
    /// the check and the change; on refusal the saved selection is restored.
    fn finish_switch(&self, state: &Arc<AppState>, target: Target, path: &Path, previous: Option<Vec<u8>>) {
        let rrls = self.config.rrls;
        let station_resets = self.config.station_resets;
        let from = crate::runtime_mode::current();
        let result = crate::runtime_mode::switch_to(
            target.mode,
            || {
                if let Some(busy) = station_busy(state, rrls) {
                    return Err(format!("Station became active before the switch: {busy}"));
                }
                self.config.target_supported(target)
            },
            |token| {
                apply_switch(state, from, token, station_resets);
                let change = json!({
                    "mode": mode_id(token.mode),
                    "label": mode_label(token.mode),
                    "policy_epoch": self.policy_epoch(),
                    "layout_revision": layout_revision(),
                });
                set_last_change(change);
            },
        );
        let mut runtime = self.runtime.lock().unwrap();
        runtime.restarting = false;
        match result {
            Ok(token) => {
                if let Some(transition) = runtime.transition.as_mut() {
                    transition.state = "applied";
                    transition.phase = format!("Switched to {}", target.label(rrls));
                    transition.error = None;
                }
                drop(runtime);
                log::warn!(
                    "Station mode switched in process: {} -> {} (epoch {})",
                    mode_id(from),
                    mode_id(token.mode),
                    token.epoch
                );
                if station_resets && (from == Mode::Testing) != (token.mode == Mode::Testing) {
                    // The calibration file differs in testing mode; the DAQ
                    // gets the reloaded one as after an operator save.
                    let calibration = state.loadcell_calibration.lock().unwrap().clone();
                    if let Err(error) = crate::web::publish_daq_calibration(state, &calibration) {
                        log::warn!("Load-cell calibration after the mode switch: {error}");
                    }
                    state.broadcast_fill_targets_snapshot();
                }
            }
            Err(error) => {
                let restored = restore_selection(path, previous);
                if let Some(transition) = runtime.transition.as_mut() {
                    transition.state = "failed";
                    transition.phase = "Switch refused; the station stays in its current mode".into();
                    transition.error = Some(match restored {
                        Ok(()) => error.clone(),
                        Err(restore) => format!(
                            "{error}. The previous selection could not be restored: {restore}. The next start may use the new selection."
                        ),
                    });
                }
                log::warn!("Station mode switch refused: {error}");
            }
        }
    }
    /// Last check before shutdown: a station that became active keeps running
    /// in its current mode and the saved selection is restored.
    fn finish_restart(&self, state: &AppState, path: &Path, previous: Option<Vec<u8>>) {
        let busy = station_busy(state, self.config.rrls);
        let mut runtime = self.runtime.lock().unwrap();
        let Some(busy) = busy else {
            drop(runtime);
            self.signal.fire();
            return;
        };
        let restored = restore_selection(path, previous);
        runtime.restarting = false;
        if let Some(transition) = runtime.transition.as_mut() {
            transition.state = "failed";
            transition.phase = "Restart cancelled; the station stays in its current mode".into();
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

fn restore_selection(path: &Path, previous: Option<Vec<u8>>) -> std::io::Result<()> {
    match previous {
        Some(bytes) => write_durable(path, &bytes),
        None => std::fs::remove_file(path).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
        }),
    }
}

/// Puts the mode-dependent state back to what a fresh start in the new mode
/// has. Runs inside `runtime_mode::switch_to` after the new mode is in force
/// and before any mode-dependent admission reopens; the station was found
/// idle under the same gate. Tasks that exist for one mode (sequence, safety,
/// paired settings, shared checklists) follow the mode epoch on their own.
fn apply_switch(state: &Arc<AppState>, from: Mode, token: crate::runtime_mode::ModeToken, station_resets: bool) {
    // HITL operator interlocks start released, as at start.
    state.hitl_button_interlock_enabled.store(false, Ordering::SeqCst);
    state.hitl_launch_interlock_enabled.store(false, Ordering::SeqCst);
    for interlock in state.hitl_safety_interlocks.iter() {
        interlock.store(false, Ordering::SeqCst);
    }
    state
        .hitl_physical_launch_uses_ground_station
        .store(false, Ordering::SeqCst);
    state.launch_sequence_command_pending.store(false, Ordering::SeqCst);
    // A testing-mode modeled retraction never survives into another session.
    crate::actuator_plumbing::clear_modeled_retraction_for_mode_switch(state);
    {
        // The sequence step starts over as at start; the generation binding
        // stays with the current recovery generation.
        let mut policy = state.sequence_policy_state.lock().unwrap();
        *policy = crate::sequences::SequencePolicyState {
            generation: policy.generation,
            ..Default::default()
        };
    }
    if station_resets {
        // Qualification state (outside HITL), HITL phase control and
        // turbocharge evidence, rebuilt as at start.
        crate::gse::mode_switch::reset_for_mode_switch(state);
        if (from == Mode::Testing) != (token.mode == Mode::Testing) {
            // Testing mode keeps its own calibration file.
            *state.loadcell_calibration.lock().unwrap() = crate::loadcell::load_or_default();
        }
    } else {
        crate::gse::mode_switch::reset_phase_control(state);
    }
    LAYOUT_REVISION.fetch_add(1, Ordering::AcqRel);
    // The policy the UI shows and the server enforces, for the new mode.
    crate::sequences::refresh_action_policy_projection(state);
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

/// Why the station is not idle enough for a mode change, if anything.
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
    if state.launch_sequence_command_pending.load(Ordering::SeqCst) {
        return Some("a Launch command is pending".into());
    }
    let flight = state.local_flight_state_snapshot();
    // Aborted is a safe resting state: a mode change sends no output command.
    if !matches!(flight, FlightState::Startup | FlightState::Idle | FlightState::Aborted) {
        return Some(format!(
            "flight state is {flight:?}; switch only from Startup, Idle or Aborted"
        ));
    }
    if state.launch_clock_snapshot().kind != crate::telemetry_db::LaunchClockKind::Idle {
        return Some("the T clock is running".into());
    }
    if let Some(reason) = crate::gse::mode_switch_block_reason(state) {
        return Some(reason.into());
    }
    if !crate::board_recovery::coordinator::manual_utc_idle(state) {
        return Some("a board recovery or reset is in progress".into());
    }
    if rrls && crate::rrls::status().forwarding {
        return Some("this RRLS is relaying recovery traffic for the primary".into());
    }
    None
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
