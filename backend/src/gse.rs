use crate::rocket_commands::{ActuatorBoardCommands, ValveBoardCommands};
pub(crate) mod cold_flow;
pub(crate) mod overpressure;
#[cfg(test)]
mod overpressure_tests;
pub(crate) mod progress;
mod turbocharge;
pub(crate) mod mode_switch;
#[cfg(all(test, feature = "firmware_sim"))]
pub(crate) use turbocharge::mass_ingress_snapshot;
pub(crate) use turbocharge::{mass_calibration_signature, observe_valve_pressure_ingress};
mod automatic_abort;
#[cfg(test)]
mod rrls_http_tests;
// Verified recovery coordinator uses the same bounded owner primitives.
pub(crate) mod reset_generation;
pub(crate) use automatic_abort::claim_automatic_abort;
pub(crate) use automatic_abort::claim_automatic_abort_for_generation;
pub(crate) use automatic_abort::{
    AutomaticAbortClaim, LivenessAbortDecision, claim_liveness_abort,
};
mod operational_policy;
use operational_policy::*;
mod operational;
use crate::{
    auth::Permission,
    sequences::{ActionControl, ActionPolicyMsg, BlinkMode},
    state::AppState,
    types::{FlightState, TelemetryCommand},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use gse_sequence::{Action, Config, Effects, Engine, Inputs, Phase, Sample, ValveState};
use gse_sequence::{
    qd_wait::{QdStatus, QdWait},
    qualification::{Fault, Milestone},
};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

// Serialize effect preparation/admission with manual handoff. Transport
// processing happens after this gate is released (callbacks may reenter GSE).
static DISPATCH_GATE: Mutex<()> = Mutex::new(());
/// Take the dispatch gate. It guards no data, so a panic while another path
/// held it leaves nothing inconsistent; poisoning must never stop a later
/// Stop, Abort or ordinary dispatch.
fn dispatch_gate() -> std::sync::MutexGuard<'static, ()> {
    DISPATCH_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
/// Nonwaiting form: None only while another owner holds the gate. A poisoned
/// gate counts as acquired, never as Busy.
fn try_dispatch_gate() -> Option<std::sync::MutexGuard<'static, ()>> {
    match DISPATCH_GATE.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

const KEYS: [u8; 5] = [0, 1, 2, 9, 10];
const OPEN: [u8; 5] = [0, 1, 2, 9, 10];
const CLOSE: [u8; 5] = [3, 4, 5, 12, 13];
const ACTIONS: [(&str, Action); 5] = [
    ("StartFill", Action::StartFill),
    ("PauseFill", Action::PauseFill),
    ("CancelFill", Action::CancelFill),
    ("ValveSelfTest", Action::SelfTest),
    ("NitrogenTest", Action::NitrogenTest),
];

/// Presentation only: retain current observed open/closed values without
/// inventing confirmation from commanded targets or expired observations.
fn completion_valve_snapshot(input: Inputs) -> [Option<bool>; 5] {
    input.valves.map(|value| {
        value
            .filter(|value| value.at_ms <= input.now_ms && input.now_ms - value.at_ms <= 2000)
            .map(|value| value.open)
    })
}

/// At most two semantic states can be admitted from one valve source tick.
/// A changed packet nonce cannot turn the same capture into a new sample.
/// At most one flip is admitted per source tick: A→B→A is conservatively
/// withheld even if it was a real second flip, because it is indistinguishable
/// from a re-nonced replay of the already-seen A capture.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ValveCaptureHistory {
    source_ms: i64,
    packet_ids: [u64; 2],
    seen_values: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct ValveReceipt {
    pub network_ms: i64,
    pub monotonic_at: Instant,
    pub simulation_ms: Option<u64>,
}

/// Prepared while the runtime mutex is held; no live evidence is changed until
/// the state-level command observation barrier admits this capture.
pub(crate) struct PreparedValveSource {
    pub age_ms: u64,
    observation: Option<(usize, ValveCaptureHistory, ValveState, u64)>,
}

impl ValveCaptureHistory {
    pub(crate) fn admit(
        &mut self,
        source_ms: i64,
        receipt_ms: i64,
        now: u64,
        open: bool,
        packet_id: u64,
        previous_at: Option<u64>,
    ) -> Option<u64> {
        if source_ms < 0 {
            return None;
        }
        let value = usize::from(open);
        let bit = 1_u8 << value;
        let same_tick = self.seen_values != 0 && self.source_ms == source_ms;
        if (same_tick && self.seen_values & bit != 0)
            || (0..2).any(|index| {
                self.seen_values & (1 << index) != 0 && self.packet_ids[index] == packet_id
            })
        {
            return None;
        }
        // Only the duplicate-timestamp part is bypassed for an unseen opposite
        // value. The unchanged UTC age, future, backwards and underflow checks
        // still run before either history or live state is committed.
        let mut previous = if same_tick || self.seen_values == 0 {
            None
        } else {
            Some(self.source_ms)
        };
        let mut at = source_sample_at(Some(source_ms), receipt_ms, now, &mut previous)?;
        if same_tick {
            let previous_at = previous_at?;
            now.checked_sub(previous_at)?;
            at = at.min(previous_at);
        }
        // Boot-time captures cannot be aged from UTC. An equal-tick exception
        // still retains its first capture anchor and the existing two-second
        // freshness bound; a late opposite value cannot rearm confirmation.
        if now.checked_sub(at)? > 2_000 {
            return None;
        }
        if !same_tick {
            self.source_ms = source_ms;
            self.seen_values = 0;
            self.packet_ids = [0; 2];
        }
        self.seen_values |= bit;
        self.packet_ids[value] = packet_id;
        Some(at)
    }
}

pub struct Runtime {
    pub engine: Engine,
    progress: crate::sequence_progress_stage::Stage,
    progress_admission: Option<LaunchAdmission>,
    progress_fill_target_kg:Option<f32>,
    cold_flow:Option<cold_flow::Session>,
    /// Operational qualification runtime; untouched (default) in HITL.
    operational: operational::Operational,
    clock: Instant,
    #[cfg(test)]
    test_now_ms: Option<u64>,
    pressure: Option<Sample>,
    valve_pressure: Option<Sample>,
    last_valve_pressure_source_ms: Option<i64>,
    valve_pressure_epoch: u64,
    turbocharge: Option<gse_sequence::turbocharge::Controller>,
    turbo_evidence: Option<gse_sequence::turbocharge::Evidence>,
    turbo_inventory_pending: bool,
    turbo_binding: Option<serde_json::Value>,
    #[cfg(test)]
    turbo_audit_path: Option<std::path::PathBuf>,
    valves: [Option<ValveState>; 5],
    mass: Option<Sample>,
    mass_calibration: Option<[u8; 32]>,
    last_mass_source_ms: Option<i64>,
    mass_source_epoch: u64,
    last_pressure_source_ms: Option<i64>,
    last_valve_captures: [ValveCaptureHistory; 5],
    fill_cutoff: crate::sequences::FillCutoff,
    fill_trim: Option<gse_sequence::fill_trim::Controller>,
    ground_station_control: bool,
    phase_control_revision: u64,
    pub(crate) manual_control: bool,
    effects_queued: bool,
    automatic_abort_in_flight: bool,
    overpressure: overpressure::Monitor,
}
impl Default for Runtime {
    fn default() -> Self {
        // RUNTIME-MODE-BOOT: the shared-checklist preflight runs when the
        // runtime is first built (boot), and only outside HITL.
        #[cfg(not(test))]
        if !crate::runtime_mode::is_hitl() {
            crate::checklists::paired::preflight();
        }
        Self {
            engine: station_engine(),
            progress: Default::default(),
            progress_admission: None,
            progress_fill_target_kg:None,
            cold_flow:None,
            operational: Default::default(),
            clock: Instant::now(),
            #[cfg(test)]
            test_now_ms: None,
            pressure: None,
            valve_pressure: None,
            last_valve_pressure_source_ms: None,
            valve_pressure_epoch: 0,
            turbocharge: None,
            turbo_evidence: None,
            turbo_inventory_pending: false,
            turbo_binding: None,
            #[cfg(test)]
            turbo_audit_path: None,
            valves: [None; 5],
            mass: None,
            mass_calibration: None,
            last_mass_source_ms: None,
            mass_source_epoch: 0,
            last_pressure_source_ms: None,
            last_valve_captures: [ValveCaptureHistory::default(); 5],
            fill_cutoff: Default::default(),
            fill_trim: None,
            ground_station_control: true,
            phase_control_revision: 0,
            manual_control: false,
            effects_queued: false,
            automatic_abort_in_flight: false,
            overpressure: Default::default(),
        }
    }
}
// Valid network UTC captures retain their source age. Unsynchronized boot
// counters retain receipt-time compatibility, but replaying the same capture
// must never manufacture a new settled sample or renew its freshness.
fn source_sample_at(
    source: Option<i64>,
    receipt: i64,
    now: u64,
    previous: &mut Option<i64>,
) -> Option<u64> {
    const UTC_2020_MS: i64 = 1_577_836_800_000;
    let Some(source) = source.filter(|v| *v >= 0) else {
        return Some(now);
    };
    if *previous == Some(source) {
        return None;
    }
    let at = if source >= UTC_2020_MS && receipt >= UTC_2020_MS {
        let delta = receipt.checked_sub(source)?;
        // Network clocks may lead by a small synchronization/receipt jitter.
        // Clamp only a bounded100 ms lead, never a materially future capture.
        if delta < -100 {
            return None;
        }
        let age = delta.max(0) as u64;
        if age > 2_000 || previous.is_some_and(|old| old >= UTC_2020_MS && source < old) {
            return None;
        }
        now.checked_sub(age)?
    } else {
        now
    };
    *previous = Some(source);
    Some(at)
}
fn scalar_capture_at(
    clock: Instant,
    processing_now: u64,
    source: Option<i64>,
    receipt: ValveReceipt,
    previous: &mut Option<i64>,
) -> Option<u64> {
    let received_at = match receipt.simulation_ms {
        Some(at) => at,
        None => u64::try_from(
            receipt
                .monotonic_at
                .checked_duration_since(clock)?
                .as_millis(),
        )
        .ok()?,
    };
    let mut next = *previous;
    let captured = source_sample_at(source, receipt.network_ms, received_at, &mut next)?;
    if processing_now.checked_sub(captured)? > 2_000 {
        return None;
    }
    *previous = next;
    Some(captured)
}
fn sequence_clock_ms(elapsed_ms: u64, simulated_ms: Option<u64>) -> u64 {
    simulated_ms.unwrap_or(elapsed_ms)
}

impl Runtime {
    /// Retire transient automation ownership after a Stop-authoritative event.
    /// Keep fault/evidence/history and real abort-worker leases unchanged.
    fn retire_automation_owner_for_stop(&mut self) {
        if let Some(mut controller) = self.fill_trim.take() { controller.cancel(); }
        if !crate::runtime_mode::is_hitl() {
            self.operational.running = false;
            self.operational.target_reached = None;
            self.operational.last_correction_ms = None;
            self.operational.qd = QdWait::default();
            self.operational.qd_launch_admission = None;
        }
    }

    fn cancel_automation_for_manual(&mut self) {
        self.progress.complete(
            crate::sequence_progress_projection::TerminalProof::Interrupted,
            crate::sequence_progress_service::now_ms(),
        );
        self.turbocharge = None;
        self.turbo_evidence = None;
        self.manual_control = true;
        let faulted = self.engine.status.phase == Phase::Fault;
        self.engine.manual_override();
        if faulted {
            self.engine.status.phase = Phase::Fault;
            self.engine.claimed = true;
        }
        self.fill_cutoff.reset();
        if let Some(mut controller) = self.fill_trim.take() {
            controller.cancel();
        }
        if !crate::runtime_mode::is_hitl() {
            self.operational.running = false;
            self.operational.target_reached = None;
            self.operational.last_correction_ms = None;
            self.operational.qd = QdWait::default();
            self.operational.qd_launch_admission = None;
        }
    }
    /// Actual valve reports and their source ages; command intent is never evidence.
    pub(crate) fn mirror_valves(&self) -> [Option<(bool, u64)>; 5] {
        let now = self.now();
        self.valves
            .map(|value| value.map(|v| (v.open, now.saturating_sub(v.at_ms))))
    }

    /// A response is current observed feedback, independent of whether the
    /// sequence engine ticked (external fill trim owns its own controller).
    /// Copy only: command targets, engine state and controller ownership stay put.
    fn presentation_status_at(
        &self,
        now_ms: u64,
        prelaunch: bool,
        interlock: bool,
    ) -> gse_sequence::Status {
        let mut status = self.engine.status.clone();
        status.valves = completion_valve_snapshot(self.input_at(now_ms, prelaunch, interlock));
        status
    }

    /// Imported calibration must not invalidate an active or already-qualified
    /// procedure. Retain it as pending until the operator returns to Idle and
    /// explicitly clears qualification. Never clear evidence automatically.
    pub fn calibration_import_allowed(&self) -> bool {
        if cold_flow::runtime_active(self) { return false; }
        if crate::runtime_mode::is_hitl() {
            false
        } else {
            self.engine.status.phase == Phase::Idle
                && !self.engine.active()
                && self.operational.saved.qualification.stage()
                    == gse_sequence::qualification::MissionStage::Idle
                && ![
                    Milestone::SelfTest,
                    Milestone::NitrogenTest,
                    Milestone::FillTarget,
                ]
                .into_iter()
                .any(|m| self.operational.saved.qualification.completed(m))
        }
    }
    pub(crate) fn checklist_definition_edit_allowed(&self) -> bool {
        self.calibration_import_allowed()
            && !self.operational.running
            && self.operational.saved.pending_checklists.is_empty()
            && self.operational.saved.qualification.fault().is_none()
    }
    pub(crate) fn checklist_template_bound(&self) -> bool {
        crate::checklists::paired::qualification_allowed(
            self.operational.saved.checklist_digest.as_deref(),
            self.operational.has_checklist_evidence(),
        )
    }
    fn fresh_mass(&self, now_ms: u64) -> Option<Sample> {
        self.mass
            .filter(|m| m.at_ms <= now_ms && now_ms - m.at_ms <= 2000)
    }
    pub fn observe_mass(&mut self, value: Option<f32>) {
        self.mass_calibration = None;
        self.last_mass_source_ms = None;
        self.mass = value.filter(|v| v.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
    pub(crate) fn observe_mass_source(
        &mut self,
        value: Option<f32>,
        source_ms: Option<i64>,
        receipt: ValveReceipt,
        calibration: [u8; 32],
    ) {
        if source_ms.is_some_and(|value| value < 0) {
            self.mass = None;
            self.mass_calibration = None;
            return;
        }
        if source_ms
            .zip(self.last_mass_source_ms)
            .is_some_and(|(new, old)| {
                new < old && (new < 1_577_836_800_000 || old < 1_577_836_800_000)
            })
        {
            self.mass_source_epoch = self.mass_source_epoch.saturating_add(1);
            self.turbo_evidence = None;
        }
        if let Some(at_ms) = scalar_capture_at(
            self.clock,
            self.now(),
            source_ms,
            receipt,
            &mut self.last_mass_source_ms,
        ) {
            self.mass_calibration = value.filter(|v| v.is_finite()).map(|_| calibration);
            self.mass = value
                .filter(|v| v.is_finite())
                .map(|value| Sample { value, at_ms });
        }
    }
    pub(crate) fn observe_pressure_source(
        &mut self,
        value: Option<f32>,
        source_ms: Option<i64>,
        receipt_ms: i64,
    ) {
        if let Some(at_ms) = source_sample_at(
            source_ms,
            receipt_ms,
            self.now(),
            &mut self.last_pressure_source_ms,
        ) {
            self.pressure = value
                .filter(|v| v.is_finite())
                .map(|value| Sample { value, at_ms });
        }
    }
    fn now(&self) -> u64 {
        #[cfg(test)]
        if let Some(now) = self.test_now_ms {
            return now;
        }
        sequence_clock_ms(
            self.clock.elapsed().as_millis() as u64,
            crate::simulation_clock_ms(),
        )
    }
    pub fn observe_pressure(&mut self, value: Option<f32>) {
        self.pressure = value.filter(|p| p.is_finite()).map(|value| Sample {
            value,
            at_ms: self.now(),
        });
    }
    #[cfg(test)]
    pub(crate) fn observe_valve_source(
        &mut self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
    ) -> Option<u64> {
        let processing_now = self.now();
        self.observe_valve_source_at(key, open, source_ms, packet_id, receipt, processing_now)
    }

    pub(crate) fn prepare_valve_source(
        &self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
    ) -> Option<PreparedValveSource> {
        self.prepare_valve_source_at(key, open, source_ms, packet_id, receipt, self.now())
    }

    pub(crate) fn commit_valve_source(&mut self, prepared: PreparedValveSource, admitted: bool) {
        if let Some((index, mut history, sample, packet_id)) = prepared.observation {
            if admitted {
                self.valves[index] = Some(sample);
            } else {
                // A pre-target tick must not gain a new receipt anchor through
                // another nonce or its opposite value. Keep only this bounded
                // replay floor; prior live safety observations stay unchanged.
                for value in 0..2 {
                    if history.seen_values & (1 << value) == 0 {
                        history.packet_ids[value] = packet_id;
                    }
                }
                history.seen_values = 3;
            }
            self.last_valve_captures[index] = history;
        }
    }

    #[cfg(test)]
    fn observe_valve_source_at(
        &mut self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
        processing_now: u64,
    ) -> Option<u64> {
        let prepared =
            self.prepare_valve_source_at(key, open, source_ms, packet_id, receipt, processing_now)?;
        let age = prepared.age_ms;
        self.commit_valve_source(prepared, true);
        Some(age)
    }

    fn prepare_valve_source_at(
        &self,
        key: u8,
        open: bool,
        source_ms: i64,
        packet_id: u64,
        receipt: ValveReceipt,
        processing_now: u64,
    ) -> Option<PreparedValveSource> {
        let Some(index) = KEYS.iter().position(|candidate| *candidate == key) else {
            return Some(PreparedValveSource {
                age_ms: 0,
                observation: None,
            });
        };
        // Use the receipt clock captured before waiting for the GSE mutex.
        // Processing or lock delay must never renew capture freshness.
        let now = match receipt.simulation_ms {
            Some(now) => now,
            None => u64::try_from(
                receipt
                    .monotonic_at
                    .checked_duration_since(self.clock)?
                    .as_millis(),
            )
            .ok()?,
        };
        let mut history = self.last_valve_captures[index];
        let at_ms = history.admit(
            source_ms,
            receipt.network_ms,
            now,
            open,
            packet_id,
            self.valves[index].map(|sample| sample.at_ms),
        )?;
        // Waiting for the mutex may have consumed the original freshness
        // budget. Do not acknowledge or poison replay history with a capture
        // that was fresh at receipt but stale (or future) at processing time.
        if processing_now.checked_sub(at_ms)? > 2_000 {
            return None;
        }
        Some(PreparedValveSource {
            age_ms: now.checked_sub(at_ms)?,
            observation: Some((index, history, ValveState { open, at_ms }, packet_id)),
        })
    }
    #[cfg(test)]
    pub fn observe_valve(&mut self, key: u8, open: bool) {
        if let Some(i) = KEYS.iter().position(|k| *k == key) {
            self.valves[i] = Some(ValveState {
                open,
                at_ms: self.now(),
            });
        }
    }
    fn input(&self, prelaunch: bool, interlock: bool) -> Inputs {
        self.input_at(self.now(),prelaunch,interlock)
    }
    /// Pure copy shared with presentation; the caller supplies the already sampled clock.
    fn input_at(&self,now_ms:u64,prelaunch:bool,interlock:bool)->Inputs {
        Inputs {
            now_ms,
            pressure: if self.turbo_inventory_pending
                && matches!(
                    self.engine.status.phase,
                    Phase::Turbocharge | Phase::PauseSetup | Phase::Paused
                ) {
                self.valve_pressure
            } else {
                self.pressure
            },
            valves: self.valves,
            prelaunch,
            interlock,
        }
    }
}
fn prelaunch(state: &AppState) -> bool {
    prelaunch_flight_state(*state.state.lock().unwrap())
}
pub(crate) fn prelaunch_flight_state(state: FlightState) -> bool {
    if crate::runtime_mode::is_hitl() && matches!(state, FlightState::Startup | FlightState::Armed) {
        return true;
    }
    matches!(
        state,
        FlightState::Idle
            | FlightState::Armed
            | FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
    )
}
fn config_path() -> std::path::PathBuf {
    std::env::var_os("GS_GSE_CONFIG")
        .map(Into::into)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gse_sequence.json")
        })
}
pub(crate) fn persist_settings(config: &Config) -> Result<(), String> {
    config.validate_settings()?;
    let path = config_path();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("upload");
    let mut persisted = config.clone();
    persisted.dry_self_test_confirmed = false;
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&persisted).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// A fresh engine for the station mode in force: test fire keeps the original
/// fill-resume deadband (formerly gse_sequence's legacy_resume_deadband
/// feature, enabled only by the test-fire build).
fn station_engine() -> Engine {
    let mut engine = Engine::default();
    engine.legacy_resume_deadband = crate::runtime_mode::is_test_fire();
    engine
}
pub fn initialize(state: &AppState) {
    turbocharge::restore_pending_inventory(state);
    restore_operational(state);
    load_config(state);
}

/// Qualification restore and the automatic-abort latch, outside HITL only.
/// Runs at boot and again after an in-process switch (mode_switch.rs).
fn restore_operational(state: &AppState) {
    if !crate::runtime_mode::is_hitl() {
        let path = std::env::var_os("GS_GSE_QUALIFICATION")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| config_path().with_file_name("gse_qualification.json"));
        let mut rt = state.gse.lock().unwrap();
        rt.operational.path = Some(path.clone());
        match operational::Saved::load(&path) {
            Ok(saved) => {
                if saved.qualification.nitrogen_satisfied() {
                    rt.engine.status.nitrogen_passed =
                        saved.qualification.completed(Milestone::NitrogenTest);
                    rt.engine.status.nitrogen_overridden =
                        saved.qualification.nitrogen_override().is_some();
                    rt.engine.status.self_test_locked = true;
                    rt.engine.status.baseline = saved.baseline.clone();
                    rt.engine.status.phase = Phase::Passed;
                    rt.engine.claimed = true;
                }
                if saved.qualification.stage() == gse_sequence::qualification::MissionStage::Fire {
                    rt.engine.claimed = true;
                    rt.engine.status.message = "Fire was already committed; manual reset is required before a new operation".into();
                }
                rt.operational.run_generation = saved.qualification.generation();
                rt.operational.saved = saved;
                if let Err(error) = rt
                    .operational
                    .persist()
                    .and_then(|()| rt.operational.flush_checklists())
                {
                    log::error!(
                        "Qualification storage unavailable; automatic actions blocked: {error}"
                    );
                }
            }
            Err(error) => {
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::MissingTelemetry);
                rt.operational.persist_error = Some(error.clone());
                log::error!("Cannot restore GSE qualification; manual reset required: {error}");
            }
        }
    }
    if !crate::runtime_mode::is_hitl() {
        let claimed = {
            let mut rt = state.gse.lock().unwrap();
            let claimed = rt.operational.saved.automatic_abort_claimed;
            if claimed {
                rt.operational.running = false;
                rt.operational.startup_configured = true;
                rt.engine.claimed = true;
                rt.engine.status.phase = Phase::Fault;
                rt.engine.status.message =
                    "Automatic abort remains latched; clear GSE abort before another operation"
                        .into();
            }
            claimed
        };
        if claimed {
            state.set_local_flight_state(FlightState::Aborted);
        }
    }
}

impl Runtime {
    /// The engine checks fill admission, its pressure ceilings and Turbocharge
    /// against its own copy of the nitrous target, which starts at its
    /// built-in 750 psi. Adopt the configured target whenever no sequence owns
    /// the engine; a running or paused session keeps the target it started
    /// with. The station's sequence loop calls this every tick in every mode;
    /// Normal and test fire also copy it on their operational tick.
    pub(crate) fn adopt_nitrous_target(&mut self, target_psi: f32) {
        if !self.engine.active() {
            self.engine.nitrous_target_psi = target_psi;
        }
    }
}
fn load_config(state: &AppState) {
    match std::fs::read(config_path())
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<Config>(&b).map_err(|e| e.to_string()))
    {
        Ok(mut config) => {
            config.dry_self_test_confirmed = false;
            state.gse.lock().unwrap().engine.config = config;
        }
        Err(err) => log::warn!(
            "GSE configuration not loaded; automatic actions require configuration: {err}"
        ),
    }
}
pub fn claimed(state: &AppState) -> bool {
    let rt = state.gse.lock().unwrap();
    rt.engine.claimed || rt.manual_control
}
pub fn active(state: &AppState) -> bool {
    state.gse.lock().unwrap().engine.active()
}
// A filled-tank hold remains Engine-active after operational Fire is
// committed. Actual pilot feedback must still advance its launch countdown.
fn pilot_clock_allowed(engine_active: bool, fire_committed: bool, hitl_mode: bool) -> bool {
    !engine_active || (!hitl_mode && fire_committed)
}
pub(crate) fn pilot_report_can_advance_launch_clock(state: &AppState) -> bool {
    let runtime = state.gse.lock().unwrap();
    if cold_flow::runtime_active(&runtime){return false;}
    let hitl = crate::runtime_mode::is_hitl();
    let fire_committed = !hitl
        && runtime.operational.saved.qualification.stage()
            == gse_sequence::qualification::MissionStage::Fire;
    pilot_clock_allowed(runtime.engine.active(), fire_committed, hitl)
}
fn action(cmd: &TelemetryCommand) -> Option<Action> {
    match cmd {
        TelemetryCommand::StartFill => Some(Action::StartFill),
        TelemetryCommand::PauseFill => Some(Action::PauseFill),
        TelemetryCommand::CancelFill => Some(Action::CancelFill),
        TelemetryCommand::ValveSelfTest => Some(Action::SelfTest),
        TelemetryCommand::NitrogenTest => Some(Action::NitrogenTest),
        _ => None,
    }
}
pub fn valve_command(cmd: &TelemetryCommand) -> bool {
    matches!(
        cmd,
        TelemetryCommand::Dump
            | TelemetryCommand::NormallyOpen
            | TelemetryCommand::Pilot
            | TelemetryCommand::Nitrogen
            | TelemetryCommand::Nitrous
            | TelemetryCommand::NitrogenClose
            | TelemetryCommand::NitrousClose
    )
}

// Safety catalog (About safety): launch-turbocharge, supply-interlock. Keep backend/src/safety_catalog_rules.rs in step.
pub fn command_allowed(state: &AppState, cmd: &TelemetryCommand) -> Option<bool> {
    if let Some(allowed) = overpressure::command_allowed(&state.gse.lock().unwrap(), cmd) {
        return Some(allowed);
    }
    if matches!(crate::sequences::command_name(cmd), "Launch" | "GroundStationLaunch" | "IgniterSequence")
        && turbocharge_active_nowait(state)
    {
        return Some(false);
    }
    let cold_flow_active = cold_flow::active(state);
    if cold_flow_active && !cold_flow::manual_handoff(cmd) {
        // An owned cold-flow run excludes competing automation, not unrelated
        // operator controls. Physical manual controls retire that run below.
        if action(cmd).is_some() || matches!(cmd, TelemetryCommand::Launch | TelemetryCommand::Turbocharge)
            || crate::sequences::command_name(cmd) == "GroundStationLaunch"
            || crate::sequences::command_name(cmd) == "IgniterSequence"
        {
            return Some(matches!(cmd,TelemetryCommand::CancelFill|TelemetryCommand::PauseFill|TelemetryCommand::Abort));
        }
    }
    if matches!(cmd, TelemetryCommand::PauseFill) && state.gse.lock().unwrap().turbocharge.is_some()
    {
        return Some(prelaunch(state) && interlock(state, &state.action_policy_snapshot()));
    }

    if matches!(cmd, TelemetryCommand::StartFill) && fill_resume_awaits_retirement(state) {
        return Some(false);
    }
    if matches!(cmd, TelemetryCommand::Turbocharge) {
        let rt = state.gse.lock().unwrap();
        let input = rt.input(
            prelaunch(state),
            interlock(state, &state.action_policy_snapshot()),
        );
        return Some(turbocharge::allowed(state, &rt, input));
    }

    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    let rt = state.gse.lock().unwrap();
    if let Some(action) = action(cmd) {
        if crate::runtime_mode::is_hitl() {
            // Admit the request, not the actuation: handle_command still
            // enforces every engine prerequisite before emitting effects. A
            // refusal the button already shows (the self-test isolation
            // confirmation) is refused here too, with its reason, instead of
            // answering "ok" and doing nothing.
            let input = rt.input(is_prelaunch, interlock(state, &policy));
            return Some(sequence_button_enabled(&rt.engine, action, input));
        }
        return Some(
            operational_action_allowed(&rt, action)
                && rt
                    .engine
                    .allows(action, rt.input(is_prelaunch, interlock(state, &policy))),
        );
    }
    if !crate::runtime_mode::is_hitl()
        && rt.operational.saved.qualification.fault().is_some()
        && valve_command(cmd)
    {
        return Some(matches!(
            cmd,
            TelemetryCommand::NitrogenClose | TelemetryCommand::NitrousClose
        ));
    }
    // A manual handoff never grants an opening that the retained physical
    // supply interlock forbids. Explicit/toggle closes remain available.
    let now = rt.now();
    let closed = |index: usize| {
        rt.valves[index].is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2_000)
    };
    if state.supply_enforcement_enabled()
        && matches!(cmd, TelemetryCommand::Nitrogen)
        && !rt.valves[3].is_some_and(|v| v.open)
        && (rt.engine.nitrous_session() || !closed(4))
    {
        return Some(false);
    }
    if state.supply_enforcement_enabled()
        && matches!(cmd, TelemetryCommand::Nitrous)
        && !rt.valves[4].is_some_and(|v| v.open)
        && !closed(3)
    {
        return Some(false);
    }
    if !crate::runtime_mode::is_hitl()
        && matches!(
            crate::sequences::command_name(cmd),
            "Launch" | "GroundStationLaunch" | "RetractPlumbing"
        )
    {
        return Some(
            is_prelaunch
                && interlock(state, &policy)
                && operational_launch_allowed(state, &rt)
                && (crate::sequences::command_name(cmd) == "RetractPlumbing"
                    || crate::runtime_mode::is_test_fire()
                    || !matches!(
                        rt.operational.qd.status(rt.now()),
                        QdStatus::NotStarted | QdStatus::AckTimedOut
                    )),
        );
    }
    if rt.engine.active()
        && !(cold_flow_active && cold_flow::manual_handoff(cmd))
        && (["GroundStationLaunch", "IgniterSequence"]
            .contains(&crate::sequences::command_name(cmd))
            || matches!(
                cmd,
                TelemetryCommand::Launch
                    | TelemetryCommand::Igniter
                    | TelemetryCommand::Postinit
                    | TelemetryCommand::RetractPlumbing
            ))
    {
        return Some(false);
    }
    None
}
// Safety catalog (About safety): key-interlock. Keep backend/src/safety_catalog_rules.rs in step.
fn interlock(state: &AppState, policy: &ActionPolicyMsg) -> bool {
    if crate::runtime_mode::is_hitl() {
        state.hitl_button_interlock_satisfied()
    } else {
        policy.key_enabled && policy.software_buttons_enabled
    }
}
fn sequence_button_enabled(engine: &Engine, action: Action, input: Inputs) -> bool {
    if crate::runtime_mode::is_hitl() {
        input.prelaunch
            && input.interlock
            && (action != Action::SelfTest
                || (engine.config.dry_self_test_confirmed
                    && !engine.status.self_test_locked
                    && !engine.active()))
    } else {
        engine.allows(action, input)
    }
}

pub fn decorate_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    let is_prelaunch = prelaunch(state);
    let fill_waits = fill_resume_awaits_retirement(state);
    let rt = state.gse.lock().unwrap();
    let input = rt.input(is_prelaunch, interlock(state, policy));
    policy.controls.retain(|c| c.cmd != "Turbocharge");
    let turbo_enabled = turbocharge::allowed(state, &rt, input);
    policy.controls.push(ActionControl {
        disabled_reason: (!turbo_enabled).then(|| {
            turbocharge::configuration_reason(&rt)
                .map(str::to_owned)
                .or_else(|| rt.engine.refusal_reason(Action::StartFill, input).filter(|_| !input.prelaunch || !input.interlock))
                .unwrap_or_else(|| "Turbocharge runs only during a fill, after the nitrogen test passes.".into())
        }),
        requested: None, network_note: None,
        cmd: "Turbocharge".into(),
        enabled: turbo_enabled,
        blink: BlinkMode::None,
        actuated: Some(rt.turbocharge.is_some()),
    });
    for (cmd, action) in ACTIONS {
        // HITL buttons accept a sequence request whenever manual controls are
        // unlocked. Execution still goes through Engine::allows/request below.
        let enabled = if action == Action::PauseFill && rt.turbocharge.is_some() {
            input.prelaunch && input.interlock
        } else {
            sequence_button_enabled(&rt.engine, action, input)
        };
        let enabled =
            enabled && (crate::runtime_mode::is_hitl() || operational_action_allowed(&rt, action));
        let waits = action == Action::StartFill && fill_waits;
        let enabled = enabled && !waits;
        policy.controls.retain(|c| c.cmd != cmd);
        // Say why it is disabled: the engine names the missing step.
        let reason = if waits {
            Some(RETIREMENT_PENDING.into())
        } else if enabled {
            None
        } else {
            Some(rt.engine.refusal_reason(action, input).unwrap_or_else(|| {
                "Not available at this step of the operational sequence.".into()
            }))
        };
        policy.controls.push(ActionControl { disabled_reason: reason, requested: None, network_note: None,
            cmd: cmd.into(),
            enabled,
            blink: BlinkMode::None,
            actuated: Some(match action {
                Action::StartFill => rt.engine.status.phase == Phase::Filling,
                Action::PauseFill => rt.engine.status.phase == Phase::Paused,
                Action::NitrogenTest => rt.engine.status.nitrogen_passed,
                _ => false,
            }),
        });
    }
    if crate::runtime_mode::is_hitl() {
        policy
            .controls
            .retain(|c| c.cmd != "ToggleGroundStationControl");
        policy.controls.push(ActionControl { disabled_reason: None, requested: None, network_note: None,
            cmd: "ToggleGroundStationControl".into(),
            enabled: true,
            blink: BlinkMode::None,
            actuated: Some(rt.ground_station_control),
        });
    }
    if rt.engine.active() {
        for c in &mut policy.controls {
            if [
                "Launch",
                "GroundStationLaunch",
                "Igniter",
                "IgniterSequence",
                "Postinit",
                "RetractPlumbing",
            ]
            .contains(&c.cmd.as_str())
            {
                c.enabled = false;
                c.blink = BlinkMode::None;
            }
        }
    }
    if !crate::runtime_mode::is_hitl() {
        let ready = input.prelaunch && input.interlock && operational_launch_allowed(state, &rt);
        for control in &mut policy.controls {
            if matches!(
                control.cmd.as_str(),
                "Launch" | "GroundStationLaunch" | "RetractPlumbing"
            ) {
                control.enabled = ready
                    && (control.cmd == "RetractPlumbing"
                        || crate::runtime_mode::is_test_fire()
                        || !matches!(
                            rt.operational.qd.status(rt.now()),
                            QdStatus::NotStarted | QdStatus::AckTimedOut
                        ));
                control.blink = if rt.operational.qd.queued() {
                    BlinkMode::Fast
                } else {
                    BlinkMode::None
                };
            }
        }
    }
}

fn apply(state: &Arc<AppState>, effects: Effects) {
    if !effects.valves.is_empty() {
        state.gse.lock().unwrap().effects_queued = true;
        if let Some(router) = state.topology_router.get() {
            for (index, open) in effects.valves {
                let kind = crate::telemetry_schema::data_type(if index < 3 {
                    "VALVE_COMMAND"
                } else {
                    "ACTUATOR_COMMAND"
                });
                let payload = if open { OPEN[index] } else { CLOSE[index] };
                let queued =
                    queue_controller_with_stop_fence(state, router, kind, &[payload | 0x80]);
                if matches!(queued, Ok(false)) {
                    continue;
                }
                if let Err(err) = queued {
                    log::error!("GSE command failed: {err}");
                    let recovery = {
                        let mut rt = state.gse.lock().unwrap();
                        let now = rt.now();
                        if !crate::runtime_mode::is_hitl() {
                            rt.operational
                                .saved
                                .qualification
                                .latch(Fault::MissingTelemetry);
                            rt.operational.running = false;
                            rt.operational.qd = QdWait::default();
                            rt.operational.qd_launch_admission = None;
                            let _ = rt.operational.persist();
                        }
                        rt.fill_trim = None;
                        rt.engine.fault(now, "Command transport failed")
                    };
                    // Best effort closes only; never continue opening a supply after an error.
                    for (index, _) in recovery.valves.into_iter().filter(|(index, _)| *index >= 3) {
                        let _ = crate::telemetry_task::queue_automatic_controller_command(
                            router,
                            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                            CLOSE[index],
                        );
                    }
                    state.add_notification(
                        "GSE command transport failed. Verify supply valves are closed.",
                    );
                    return;
                }
                log::info!(
                    "GSE command queued phase={:?} valve={} requested={} type={} payload={:02x}",
                    state.gse.lock().unwrap().engine.status.phase,
                    ["Pilot", "Vent", "Dump", "Nitrogen", "Nitrous"][index],
                    if open { "open" } else { "closed" },
                    if index < 3 {
                        "VALVE_COMMAND"
                    } else {
                        "ACTUATOR_COMMAND"
                    },
                    payload,
                );
                state.set_pending_umbilical_valve_state(KEYS[index], open);
            }
        } else {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            if !crate::runtime_mode::is_hitl() {
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::TopologyLost);
                rt.operational.running = false;
                let _ = rt.operational.persist();
            }
            rt.engine
                .fault(now, "No hardware command router is available");
            drop(rt);
            state.add_notification("GSE sequence failed: no hardware command router is available");
            return;
        }
    }
    if let Some(message) = effects.notification {
        // Completed state transitions are informational, not unresolved alarms.
        // Keep faults persistent until acknowledged, but do not replay ordinary
        // pause/cancel/pass/self-test notices every time a client reconnects.
        let persistent = state.gse.lock().unwrap().engine.status.phase == Phase::Fault
            || message.starts_with(gse_sequence::FAILURE_PAUSE_PREFIX);
        state.add_notification_with_persistence(message, persistent);
    }
}
pub fn handle_command(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    handle_command_with_motion_origin(state, cmd, None)
}
pub(crate) fn handle_command_with_motion_origin(
    state: &Arc<AppState>,
    cmd: &TelemetryCommand,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
) -> bool {
    handle_command_with_origins(state, cmd, plumbing_origin, None)
}
pub(crate) fn handle_command_with_origins(
    state: &Arc<AppState>,
    cmd: &TelemetryCommand,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
) -> bool {
    let automatic_observation = crate::automatic_flight_state::Observation::begin(state);
    if cold_flow::active(state) && matches!(cmd,TelemetryCommand::CancelFill|TelemetryCommand::PauseFill) {
        cold_flow::cancel_or_park_current(state,matches!(cmd,TelemetryCommand::PauseFill));
        return true;
    }
    let handled = {
        let _dispatch = dispatch_gate();
        handle_command_inner(state, cmd, plumbing_origin, launch_revision)
    };
    if let Some(observation) = automatic_observation.as_ref() {
        crate::sequences::refresh_action_policy_with_observation(state, observation);
    } else {
        crate::sequences::refresh_action_policy_projection(state);
    }
    flush_effects(state);
    progress::flush(state);
    handled
}
fn flush_effects(state: &Arc<AppState>) {
    if !std::mem::take(&mut state.gse.lock().unwrap().effects_queued) {
        return;
    }
    if let Some(router) = state.topology_router.get() {
        crate::telemetry_task::flush_command_tx(router, "GSE sequence");
    }
}
/// Pure classification before recording I/O. No run, tare, progress or valve
/// state is changed. The same classification is checked again after the await.
pub(crate) fn classify_fill_for_recording(
    state: &AppState,
    origin: &crate::fill_recording::Origin,
) -> Result<crate::fill_recording::Disposition, crate::fill_recording::Error> {
    let _dispatch = dispatch_gate();
    classify_fill_for_recording_under_gate(state, origin, true)
}
fn classify_fill_for_recording_under_gate(
    state: &AppState,
    origin: &crate::fill_recording::Origin,
    preparation: bool,
) -> Result<crate::fill_recording::Disposition, crate::fill_recording::Error> {
    use crate::fill_recording::{Disposition, Error};
    origin.live()?;
    if preparation {
        origin.remaining()?;
    }
    if state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0
        || with_stop_admission(state, origin.ticket(), || ()).is_err()
    { return Err(Error::Stale); }
    match manual_stop_state(state) {
        CurrentState::Current => {},
        CurrentState::PendingBusy => return Err(Error::PendingBusy),
        CurrentState::Invalid => return Err(Error::Stale),
    }
    if cold_flow::active(state) {
        return Err(Error::Busy);
    }
    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    let targets = state.fill_targets_snapshot().nitrous;
    let zero_pending = {
        let zero = state.auto_zero.lock().unwrap();
        zero.fill_capture_pending()
    };
    let rt = state.gse.lock().unwrap();
    let input = rt.input(is_prelaunch, interlock(state, &policy));
    if !input.prelaunch || !input.interlock {
        return Err(Error::Stale);
    }
    if matches!(rt.engine.status.phase, Phase::FillSetup | Phase::Filling) || zero_pending {
        return Ok(Disposition::AlreadyFilling);
    }
    if !crate::runtime_mode::is_hitl() && !operational_action_allowed(&rt, Action::StartFill) {
        return Err(Error::Stale);
    }
    if rt.ground_station_control
        && rt.valve_pressure.is_some_and(|p| {
            p.value >= targets.target_pressure_psi
                && p.at_ms <= input.now_ms
                && input.now_ms - p.at_ms <= 2000
        })
    {
        return Err(Error::Stale);
    }
    match rt
        .engine
        .fill_resume_required(input, rt.fresh_mass(input.now_ms), targets.target_mass_kg)
    {
        Ok(false) => Ok(Disposition::FullNoop),
        Ok(true) => Ok(Disposition::MayStart),
        Err(reason) => Err(Error::Validation(reason)),
    }
}
/// This helper admits only the first bounded step, then the existing zero
/// controller owns its unchanged capture deadline. No hot lock spans begin().
pub(crate) fn apply_recorded_fill(
    state: &Arc<AppState>,
    prepared: std::sync::Arc<crate::fill_recording::Prepared>,
) -> Result<(), crate::fill_recording::Error> {
    use crate::fill_recording::{Disposition, Error};
    {
        let _dispatch = dispatch_gate();
        let now = classify_fill_for_recording_under_gate(state, &prepared.origin, true)?;
        if !state.fill_recording.current(&prepared.receipt) {
            return Err(Error::Unavailable);
        }
        if prepared.initial == Disposition::FullNoop && now != Disposition::FullNoop {
            return Err(Error::Stale);
        }
        if prepared.initial == Disposition::AlreadyFilling || now == Disposition::AlreadyFilling {
            return Ok(());
        }
        if now == Disposition::FullNoop {
            state.gse.lock().unwrap().engine.status.message =
                "Fill unchanged: weight is within 0.2 kg of the configured target or above it"
                    .into();
            return Ok(());
        }
        *state.fill_recording.continuation.lock().unwrap() = Some(prepared.clone());
    }
    if !state.fill_recording.current(&prepared.receipt) { return Err(Error::Stale); }
    match prepared.origin.ram_current(state) {
        CurrentState::Current => {},
        CurrentState::PendingBusy => return Err(Error::PendingBusy),
        CurrentState::Invalid => return Err(Error::Stale),
    }
    let cmd = TelemetryCommand::StartFill;
    state.record_command_accepted(&cmd, crate::telemetry_task::get_current_timestamp_ms());
    state.auto_zero.lock().unwrap().cancel_for_command(&cmd);
    if crate::auto_zero::before_fill(state, &cmd) {
        return Ok(());
    }
    {
        let _dispatch = dispatch_gate();
        let now = classify_fill_for_recording_under_gate(state, &prepared.origin, true)?;
        if !state.fill_recording.current(&prepared.receipt) {
            return Err(Error::Unavailable);
        }
        if now == Disposition::FullNoop {
            state.fill_recording.continuation.lock().unwrap().take();
            return Ok(());
        }
        if now != Disposition::MayStart {
            return Err(Error::Stale);
        }
        // A held SDK core decided nothing about earlier commands' retirement;
        // the recorded Fill owner retries instead of the engine refusing it.
        if fill_resume_retirement(state).is_err() {
            return Err(Error::PendingBusy);
        }
        handle_command_inner(state, &cmd, None, None);
        state.fill_recording.continuation.lock().unwrap().take();
    }
    flush_effects(state);
    progress::flush(state);
    Ok(())
}

/// Consume only an already-safe, explicit no-op before unrelated capture cancellation.
/// The existing dispatch claim remains owned by the caller; no effects or progress start.
pub(crate) fn consume_fill_noop(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    if !matches!(cmd, TelemetryCommand::StartFill) {
        return false;
    }
    let _dispatch = dispatch_gate();
    if cold_flow::active(state) {
        return false;
    }
    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    let targets = state.fill_targets_snapshot().nitrous;
    let mut rt = state.gse.lock().unwrap();
    if !crate::runtime_mode::is_hitl() && !operational_action_allowed(&rt, Action::StartFill) {
        return false;
    }
    let input = rt.input(is_prelaunch, interlock(state, &policy));
    if rt.ground_station_control && rt.valve_pressure.is_some_and(|p| {
        p.value >= targets.target_pressure_psi
            && p.at_ms <= input.now_ms
            && input.now_ms - p.at_ms <= 2000
    }) {
        return false;
    }
    if !matches!(rt.engine.fill_resume_required(input, rt.fresh_mass(input.now_ms), targets.target_mass_kg), Ok(false)) {
        return false;
    }
    rt.engine.status.message = "Fill unchanged: weight is within 0.2 kg of the configured target or above it".into();
    true
}

fn handle_command_inner(
    state: &Arc<AppState>,
    cmd: &TelemetryCommand,
    plumbing_origin: Option<crate::actuator_plumbing::MotionOrigin>,
    launch_revision: Option<u64>,
) -> bool {
    if overpressure::command_allowed(&state.gse.lock().unwrap(), cmd) == Some(false) {
        state.add_temporary_notification(
            "Blocked: a tank overpressure error holds dump and vent open; acknowledge it after pressure drops",
        );
        return true;
    }
    if matches!(cmd, TelemetryCommand::Abort) {
        // Engine may already be Fault/inactive while its operational owner is
        // still retained. Retire it even on that path and before ColdFlow's
        // early return; this emits no outputs or additional Abort packets.
        state.gse.lock().unwrap().retire_automation_owner_for_stop();
    }
    if cold_flow::handle_under_dispatch(state,cmd){return true;}
    if matches!(cmd, TelemetryCommand::Turbocharge) {
        turbocharge::request(state);
        return true;
    }
    if matches!(
        cmd,
        TelemetryCommand::PauseFill | TelemetryCommand::CancelFill | TelemetryCommand::Abort
    ) {
        let mut rt = state.gse.lock().unwrap();
        let turbo_active = rt.turbocharge.take().is_some();
        if turbo_active {
            rt.turbo_evidence = None;
            if matches!(
                cmd,
                TelemetryCommand::PauseFill | TelemetryCommand::CancelFill
            ) {
                rt.engine.status.phase = Phase::Filling;
            }
        } else if !matches!(cmd, TelemetryCommand::PauseFill) {
            rt.turbo_evidence = None;
        }
    }
    // Admission and handoff share the effect gate: a supply report may have
    // changed since the outer command worker checked the policy.
    if valve_command(cmd) && !state.is_command_allowed(cmd) {
        state.add_temporary_notification(
            "Manual valve command blocked: current interlock or supply safety state changed",
        );
        return true;
    }
    if !crate::runtime_mode::is_hitl()
        && handle_operational_command(state, cmd, plumbing_origin, launch_revision)
    {
        return true;
    }
    if crate::runtime_mode::is_hitl() && matches!(cmd, TelemetryCommand::ToggleGroundStationControl) {
        let enabled = {
            let mut rt = state.gse.lock().unwrap();
            rt.ground_station_control = !rt.ground_station_control;
            state.phase_control_selected.store(rt.ground_station_control,std::sync::atomic::Ordering::Release);
            rt.phase_control_revision = rt.phase_control_revision.saturating_add(1);
            state.phase_control_revision.store(rt.phase_control_revision,std::sync::atomic::Ordering::Release);
            if !rt.ground_station_control && rt.fill_trim.is_some() {
                rt.cancel_automation_for_manual();
                rt.engine.status.message =
                    "Automatic mass trim disabled; operator controls the current valve states"
                        .into();
            }
            rt.fill_cutoff.reset();
            rt.ground_station_control
        };
        state.add_temporary_notification(if enabled {
            "Ground station automatic fill cutoff enabled"
        } else {
            "Ground station automatic fill cutoff disabled; operator must stop filling"
        });
        crate::sequences::refresh_action_policy_under_dispatch(state);
        return true;
    }
    if let Some(action) = action(cmd) {
        // Capture ownership outside GSE. Run-key reservation below is atomic only,
        // and happens after actual non-noop engine admission.
        let progress_admission = LaunchAdmission::capture(state);
        let is_prelaunch = prelaunch(state);
        let policy = state.action_policy_snapshot();
        let nitrous_target = state.fill_targets_snapshot().nitrous.target_pressure_psi;
        let progress_mass_target=state.fill_targets_snapshot().nitrous.target_mass_kg;
        let fill_retirement = if action == Action::StartFill { fill_resume_retirement(state) } else { Ok(false) };
        let fill_waits = fill_retirement != Ok(false);
        let fill_waits_busy = fill_retirement.is_err();
        let result = {
            let mut rt = state.gse.lock().unwrap();
            if !crate::runtime_mode::is_hitl() && !operational_action_allowed(&rt, action) {
                rt.engine.status.message =
                    "Complete prerequisites in order and wait for stable fill-system routes".into();
                return true;
            }
            if action == Action::StartFill && !rt.engine.active() {
                rt.engine.nitrous_target_psi = nitrous_target;
            }
            let input = rt.input(is_prelaunch, interlock(state, &policy));
            if matches!(action, Action::PauseFill | Action::CancelFill) && rt.fill_trim.is_some() {
                if let Some(mut controller) = rt.fill_trim.take() {
                    controller.cancel();
                }
                rt.engine.status.phase = Phase::Filling;
            }
            if action == Action::StartFill
                && rt.ground_station_control
                && rt.valve_pressure.is_some_and(|p| {
                    p.value >= nitrous_target
                        && p.at_ms <= input.now_ms
                        && input.now_ms - p.at_ms <= 2000
                })
            {
                Err(
                    "Nitrous pressure is already at its target; automatic fill remains paused"
                        .into(),
                )
            } else if action == Action::StartFill
                && rt.ground_station_control
                && rt.fresh_mass(input.now_ms).is_none()
            {
                Err("Automatic fill requires fresh calibrated fill weight before starting".into())
            } else if action == Action::StartFill
                && fill_waits
                && fill_resume_candidate(&rt, input, progress_mass_target)
            {
                // A held SDK core decided nothing; the operator's Resume retries.
                Err(if fill_waits_busy { ROUTER_CORE_BUSY } else { RETIREMENT_PENDING }.into())
            } else {
                let previous_phase = rt.engine.status.phase;
                let resuming_fill = previous_phase == Phase::Paused
                    && rt.progress.current().is_some_and(|s| {
                        s.kind == crate::sequence_progress::Kind::Fill && !s.outcome.terminal()
                    });
                let mass = rt.fresh_mass(input.now_ms);
                let result = if action == Action::StartFill {
                    match rt.engine.request_fill(input, mass, progress_mass_target) {
                        Ok(gse_sequence::fill_resume::FillStart::Noop) => {
                            rt.engine.status.message = "Fill unchanged: weight is within 0.2 kg of the configured target or above it".into();
                            return true;
                        }
                        Ok(gse_sequence::fill_resume::FillStart::Started(effects)) => Ok(effects),
                        Err(error) => Err(error),
                    }
                } else {
                    rt.engine.request(action, input)
                };
                if result.is_ok() {
                    let reserved = state
                        .sequence_progress
                        .reserve(progress_admission.generation)
                        .ok()
                        .map(|key| (key, progress_admission));
                    if action == Action::StartFill {
                        rt.progress_fill_target_kg = Some(progress_mass_target);
                    }
                    rt.manual_control = false;
                    use crate::{sequence_progress::Kind, sequence_progress_projection::Cause};
                    let kind = match action {
                        Action::SelfTest => Some(Kind::ValveSelfTest),
                        Action::NitrogenTest => Some(Kind::NitrogenTest),
                        Action::StartFill if !resuming_fill => Some(Kind::Fill),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        rt.progress_start(reserved, kind, input);
                    } else {
                        rt.progress_observe(
                            input,
                            match action {
                                Action::PauseFill => Cause::PauseAccepted,
                                Action::CancelFill => Cause::CancelAccepted,
                                _ => Cause::ResumeAccepted,
                            },
                        );
                    }
                }
                if !crate::runtime_mode::is_hitl() && result.is_ok() {
                    rt.operational.running = true;
                    rt.operational.run_generation = rt.operational.saved.qualification.generation();
                    rt.operational.target_reached = None;
                    if action == Action::StartFill
                        && let Err(error) = rt.operational.begin_fill() {
                            rt.operational.running = false;
                            return {
                                let effects = pause_for_failure(
                                    &mut rt,
                                    input,
                                    &format!("Fill stage could not be saved: {error}"),
                                );
                                drop(rt);
                                apply(state, effects);
                                true
                            };
                        }
                }
                result
            }
        };
        match result {
            Ok(effects) => apply(state, effects),
            Err(err) => {
                state.gse.lock().unwrap().engine.status.message = err.clone();
                state.add_notification(err);
            }
        }
        crate::sequences::refresh_action_policy_under_dispatch(state);
        return true;
    }
    if matches!(cmd, TelemetryCommand::Abort) && active(state) {
        let effects = {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            rt.fill_trim = None;
            rt.engine.fault(now, "Operator abort")
        };
        apply(state, effects);
    } else if valve_command(cmd) {
        let mut rt = state.gse.lock().unwrap();
        rt.cancel_automation_for_manual();
    }
    false
}
/// The operator's Dump choice at a nitrogen-test hold. Caller holds DISPATCH
/// and has validated the run key and ownership epoch. The same test run
/// continues until the tank reaches its zero band (Cancel ends it at once).
pub(super) fn dump_test_hold_under_dispatch(state: &Arc<AppState>) -> Result<(), String> {
    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    let interlock = interlock(state, &policy);
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        let input = rt.input(is_prelaunch, interlock);
        let effects = rt.engine.dump_test_hold(input)?;
        rt.manual_control = false;
        if !crate::runtime_mode::is_hitl() {
            rt.operational.running = true;
        }
        rt.progress_observe(input, crate::sequence_progress_projection::Cause::Tick);
        effects
    };
    apply(state, effects);
    crate::sequences::refresh_action_policy_under_dispatch(state);
    Ok(())
}
/// Refusal shown wherever a resume waits for earlier valve commands to retire.
pub(crate) const RETIREMENT_PENDING: &str =
    "Earlier valve commands are still being delivered; resume after they retire";
/// A StartFill that would actually resume a paused fill session (not a 0.2 kg
/// no-op) must wait for the same SDK command retirement as the modal Resume.
fn fill_resume_awaits_retirement(state: &AppState) -> bool {
    // A held SDK core is never retired.
    fill_resume_retirement(state) != Ok(false)
}
/// Whether `command_allowed` refused only because a StartFill resume probe
/// found the SDK core held (nothing decided).
pub(crate) fn command_gate_core_busy(state: &AppState, cmd: &TelemetryCommand) -> bool {
    matches!(cmd, TelemetryCommand::StartFill | TelemetryCommand::FillWithRecording { .. })
        && fill_resume_retirement(state).is_err()
}
/// Probes the SDK only when a StartFill would actually resume a paused fill.
/// Err while the SDK core was held (nothing decided).
fn fill_resume_retirement(state: &AppState) -> Result<bool, sedsnet::router::TryPendingTypeError> {
    let target_kg = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let input = {
        let policy = state.action_policy_snapshot();
        (prelaunch(state), interlock(state, &policy))
    };
    let candidate = {
        let rt = state.gse.lock().unwrap();
        let input = rt.input(input.0, input.1);
        fill_resume_candidate(&rt, input, target_kg)
    };
    if !candidate {
        return Ok(false);
    }
    try_commands_retired(state).map(|retired| !retired)
}
fn fill_resume_candidate(rt: &Runtime, input: Inputs, target_kg: f32) -> bool {
    rt.engine.status.phase == Phase::Paused
        && rt.engine.nitrous_session()
        && rt.fill_trim.is_none()
        && rt.turbocharge.is_none()
        && rt
            .engine
            .fill_resume_required(input, rt.fresh_mass(input.now_ms), target_kg)
            .is_ok_and(|required| required)
}
/// Retirement barrier: no valve or actuator command (from any producer) is
/// still owned by the command SDK, so none can be transmitted or retried
/// later and overtake a pause closure. A busy SDK core is never retired.
pub(super) fn commands_retired(state: &AppState) -> bool {
    matches!(try_commands_retired(state), Ok(true))
}
/// Err only while another thread holds the SDK core: nothing was decided, so
/// an operator Resume may check again instead of reporting delivery pending.
pub(super) fn try_commands_retired(state: &AppState) -> Result<bool, sedsnet::router::TryPendingTypeError> {
    // Same as `manual_stop_pending`, but a held core is reported, not waited on.
    if manual_stop_ram_pending(state) {
        return Ok(false);
    }
    let Some(router) = state.topology_router.get() else { return Ok(false); };
    if crate::abort_retirement::try_has_pending_reliable_tx_type(router, crate::telemetry_schema::data_type("ABORT"))? {
        return Ok(false);
    }
    for name in ["VALVE_COMMAND", "ACTUATOR_COMMAND"] {
        if router.try_has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name))? {
            return Ok(false);
        }
    }
    Ok(true)
}
/// Resume the paused valve self-test or nitrogen test. Caller holds DISPATCH
/// and has validated the run key and ownership epoch.
pub(super) fn resume_test_under_dispatch(state: &Arc<AppState>) -> Result<(), String> {
    let is_prelaunch = prelaunch(state);
    let policy = state.action_policy_snapshot();
    match try_commands_retired(state) {
        Ok(true) => {}
        Ok(false) => return Err(RETIREMENT_PENDING.into()),
        Err(_) => return Err(ROUTER_CORE_BUSY.into()),
    }
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        let paused = rt.engine.paused_test().ok_or("No paused test")?;
        if !crate::runtime_mode::is_hitl() {
            let action = match paused {
                gse_sequence::PausedTest::ValveSelfTest => Action::SelfTest,
                gse_sequence::PausedTest::NitrogenTest => Action::NitrogenTest,
            };
            if !operational_action_allowed(&rt, action) {
                return Err(
                    "Complete prerequisites in order and wait for stable fill-system routes".into(),
                );
            }
            // Completion is bound to the run's original qualification generation.
            if rt.operational.run_generation != rt.operational.saved.qualification.generation() {
                return Err(
                    "Qualification was reset during the pause; cancel and start a new test".into(),
                );
            }
        }
        let input = rt.input(is_prelaunch, interlock(state, &policy));
        let effects = rt.engine.resume_test(input)?;
        rt.manual_control = false;
        rt.progress_observe(input, crate::sequence_progress_projection::Cause::ResumeAccepted);
        if !crate::runtime_mode::is_hitl() {
            rt.operational.running = true;
            rt.operational.target_reached = None;
        }
        effects
    };
    apply(state, effects);
    crate::sequences::refresh_action_policy_under_dispatch(state);
    Ok(())
}
fn fill_mass_in_band(config: &Config, mass: f32, target: f32) -> bool {
    mass.is_finite()
        && target.is_finite()
        && target != 0.0
        && (mass - target).abs() <= config.fill_tolerance_kg
}
fn fill_stop_reached(config: &Config, mass: f32, target: f32) -> bool {
    mass.is_finite()
        && target.is_finite()
        && target != 0.0
        && mass * target.signum() >= target.abs() + config.fill_overfill_kg
}
fn remember_automatic_cutoff_target(runtime: &mut Runtime, mass: f32, target: f32) {
    // The stop and qualification candidate must agree on the existing target
    // tolerance. A pressure plateau alone must never qualify a short fill.
    if fill_mass_in_band(&runtime.engine.config, mass, target) {
        runtime.operational.target_reached = Some(target);
    }
}
/// Admit a late asynchronous fill completion through the same launch gate.
pub(crate) fn start_fill_after_automatic_zero(state: &Arc<AppState>, generation: u64) {
    let _claim = {
        let _dispatch = dispatch_gate();
        let Some(claim) = claim_operator_dispatch_under_gate(state, generation) else {
            return;
        };
        let Some(prepared) = state.fill_recording.continuation.lock().unwrap().clone() else {
            return;
        };
        if prepared.origin.generation() != generation
            || prepared.origin.live().is_err()
            || !state.fill_recording.current(&prepared.receipt)
            || manual_stop_pending(state)
            || !automatic_abort_allowed(state)
            || with_stop_admission(state, prepared.origin.ticket(), || ()).is_err()
        {
            return;
        }
        {
            let zero = state.auto_zero.lock().unwrap();
            let token = prepared
                .zero_generation
                .load(std::sync::atomic::Ordering::Acquire);
            if token == 0 || !zero.completed_fill_capture(token) {
                return;
            }
        }
        // Consume this exact accepted continuation once, before any effects.
        let mut continuation = state.fill_recording.continuation.lock().unwrap();
        if !continuation
            .as_ref()
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, &prepared))
        {
            return;
        }
        continuation.take();
        drop(continuation);
        state.gse.lock().unwrap().observe_mass(Some(0.0));
        *state.latest_fill_mass_kg.lock().unwrap() = Some(0.0);
        handle_command_inner(state, &TelemetryCommand::StartFill, None, None);
        claim
    };
    flush_effects(state);
    progress::flush(state);
}

pub(crate) fn queue_automatic_supply_close(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    command: u8,
    generation: u64,
) -> sedsnet::TelemetryResult<bool> {
    let _dispatch = dispatch_gate();
    let Some(_claim) = claim_operator_dispatch_under_gate(state, generation) else {
        return Ok(false);
    };
    if generation != automatic_generation(state) || !automatic_abort_allowed(state) {
        return Ok(false);
    }
    crate::telemetry_task::queue_automatic_controller_command(
        router,
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        command,
    )?;
    Ok(true)
}

/// A sequence failure that is not a physical hazard. Retire any trim or
/// Turbocharge controller, close both supplies, then every valve, and hold
/// Paused with fresh board confirmation for the operator (Resume, Cancel or
/// manual valves). Never opens vent or dump and never latches a
/// qualification fault; Launch stays refused until the operator acknowledges.
pub(super) fn pause_for_failure(rt: &mut Runtime, input: Inputs, reason: &str) -> Effects {
    if let Some(mut controller) = rt.fill_trim.take() {
        controller.cancel();
    }
    if rt.turbocharge.take().is_some() {
        rt.turbo_evidence = None;
    }
    let effects = rt.engine.pause_for_failure(input.now_ms, reason);
    if rt.engine.status.phase == Phase::PauseSetup {
        if !crate::runtime_mode::is_hitl() {
            rt.operational.running = true;
            rt.operational.target_reached = None;
        }
        rt.progress_observe(
            input,
            crate::sequence_progress_projection::Cause::PauseAccepted,
        );
    }
    effects
}

/// Recoverable/unclassified faults pause an active sequence closed instead of
/// latching the entire vehicle aborted or dumping the tank. Serialize with
/// launch so no delayed pause can close a supply after Fire admission.
pub(crate) fn pause_fill_for_recoverable_fault(state: &Arc<AppState>, reason: &str) {
    let generation = capture_operator_origin(state).unwrap_or(0);
    pause_fill_for_recoverable_fault_for_generation(state, reason, generation);
}

pub(crate) fn pause_fill_for_recoverable_fault_for_generation(
    state: &Arc<AppState>,
    reason: &str,
    generation: u64,
) {
    let _claim = {
        let _dispatch = dispatch_gate();
        let Some(claim) = claim_operator_dispatch_under_gate(state, generation) else {
            return;
        };
        if !automatic_abort_allowed(state) {
            return;
        }
        if prelaunch(state) {
            let policy = state.action_policy_snapshot();
            let interlock = interlock(state, &policy);
            let effects = {
                let mut rt = state.gse.lock().unwrap();
                let active = rt.engine.active() || rt.fill_trim.is_some() || rt.turbocharge.is_some();
                active.then(|| {
                    let input = rt.input(true, interlock);
                    pause_for_failure(&mut rt, input, reason)
                })
            };
            if let Some(effects) = effects {
                apply(state, effects);
            }
        }
        claim
    };
    flush_effects(state);
    progress::flush(state);
}

pub fn tick(state: &Arc<AppState>, interlock: bool) {
    let configured = state.fill_targets_snapshot().nitrous.target_pressure_psi;
    state.gse.lock().unwrap().adopt_nitrous_target(configured);
    // Overpressure precedes cold flow, sequences, pause and hold logic.
    overpressure::evaluate(state);
    if state.gse.lock().unwrap().overpressure.owns_valves() {
        flush_effects(state);
        return;
    }
    let automatic_observation = crate::automatic_flight_state::Observation::begin(state);
    if cold_flow::active(state) {
        let Some(_claim)=claim_operator_dispatch(state,automatic_generation(state))else{return;};
        cold_flow::tick(state,interlock);
        progress::flush(state);
        return;
    }
    let _claim = {
        let _dispatch = dispatch_gate();
        let Some(claim) = claim_operator_dispatch_under_gate(state, automatic_generation(state))
        else {
            return;
        };
        tick_inner(state, interlock);
        claim
    };
    if !crate::runtime_mode::is_hitl()
        && let Some(observation) = automatic_observation.as_ref()
    {
        publish_qualified_flight_armed_with_observation(state, observation);
    }
    flush_effects(state);
    progress::flush(state);
}
fn tick_inner(state: &Arc<AppState>, interlock: bool) {
    if state
        .launch_history
        .lock()
        .unwrap()
        .automatic_actions_inhibited()
    {
        return;
    }
    if turbocharge::tick(state, interlock) {
        return;
    }
    if turbocharge::guard_closed_pressure(state) {
        return;
    }
    let hitl = crate::runtime_mode::is_hitl();
    if !hitl && operational_tick(state, interlock) {
        return;
    }
    let is_prelaunch = prelaunch(state);
    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let effects = {
        let mut rt = state.gse.lock().unwrap();
        let input = rt.input(is_prelaunch, interlock);
        if rt
            .progress
            .current()
            .is_some_and(|s| s.kind == crate::sequence_progress::Kind::Fill)
        {
            rt.progress_fill_target_kg = Some(target);
        }
        let previous_phase = rt.engine.status.phase;
        // Overfill relief belongs to automatic fill only.
        rt.engine.manual_fill_control = !rt.ground_station_control || rt.manual_control;
        if let Some(mut controller) = rt.fill_trim.take() {
            let mass = rt.fresh_mass(input.now_ms);
            let update = controller.tick(input, mass);
            let reporting_trim = controller.progress_observation();
            let reporting_view = rt.engine.progress_observation(input);
            rt.progress.observe_trim(
                reporting_trim,
                reporting_view,
                mass,
                input.now_ms,
                update.fault.as_deref(),
                crate::sequence_progress_service::now_ms(),
            );
            if let Some(message) = update.fault.clone().filter(|_| update.hazard) {
                if !hitl {
                    rt.operational
                        .saved
                        .qualification
                        .latch(Fault::ValveMismatch);
                    rt.operational.running = false;
                    let _ = rt.operational.persist();
                }
                // Backend owns the established emergency relief policy for a
                // hazard: close both supplies and open vent/dump without ACK gating.
                rt.engine.fault(input.now_ms, &message)
            } else if let Some(message) = update.fault {
                // Every other trim failure pauses closed for the operator.
                pause_for_failure(&mut rt, input, &message)
            } else if update.completed {
                let inventory_retired=turbocharge::liquid_inventory_requalified(&mut rt);
                if let Err(error) = inventory_retired {
                    rt.engine.status.message = format!(
                        "Fill closed; old Turbocharge inventory record could not be retired: {error}"
                    );
                    rt.progress_failed_current();
                }
                // The trim, not the engine, closed the valves: hold Paused with
                // that closed configuration as the engine's command, or a
                // paused tick checks the fill's earlier vent/nitrous opening.
                rt.engine.hold_settled_fill(
                    input,
                    "Fill mass settled within tolerance; all valves confirmed closed",
                );
                rt.engine.status.valves = completion_valve_snapshot(input);
                if !hitl {
                    if let Some(mass) = mass {
                        remember_automatic_cutoff_target(&mut rt, mass.value, target);
                    }
                    complete_operational_milestones(&mut rt, Phase::PauseSetup, input);
                } else {
                    rt.progress.complete(
                        crate::sequence_progress_projection::TerminalProof::VerifiedFillTrim {
                            trim: reporting_trim,
                            input,
                        },
                        crate::sequence_progress_service::now_ms(),
                    );
                }
                let mut effects = update.effects;
                effects.notification = Some("Fill complete: settled mass is within tolerance and all valves are confirmed closed".into());
                effects
            } else {
                rt.engine.status.message =
                    format!("Final fill mass verification: {:?}", controller.phase());
                rt.fill_trim = Some(controller);
                update.effects
            }
        } else {
            let progress_before = hitl.then(|| rt.engine.progress_observation(input));
            let phase_before = rt.engine.status.phase;
            let effects = turbocharge::pressure_cutoff(&mut rt, input)
                .unwrap_or_else(|| rt.engine.tick(input));
            // A failed nitrogen test that the operator dumped ends Failed.
            if phase_before == Phase::Dumping && rt.engine.status.phase == Phase::Cancelled {
                rt.progress_failed_current();
            }
            if let Some(progress_before) = progress_before {
                let after = rt.engine.progress_observation(input);
                rt.progress_observe(input, crate::sequence_progress_projection::Cause::Tick);
                let kind = match (progress_before.phase, after.phase) {
                    (Phase::SelfFinish, Phase::Idle) => {
                        Some(crate::sequence_progress::Kind::ValveSelfTest)
                    }
                    (Phase::Dumping, Phase::Passed) => {
                        Some(crate::sequence_progress::Kind::NitrogenTest)
                    }
                    _ => None,
                };
                if let Some(kind) = kind {
                    rt.progress.complete(
                        crate::sequence_progress_projection::TerminalProof::EngineCompleted {
                            kind,
                            before: progress_before,
                            after,
                        },
                        crate::sequence_progress_service::now_ms(),
                    );
                }
            }
            if !hitl {
                // A long step (a dump to zero, a hold) changes no phase; keep
                // its live watch on the current tank and valve reports, as
                // HITL does above.
                if rt.engine.status.phase == previous_phase && rt.engine.active() {
                    rt.progress_observe(input, crate::sequence_progress_projection::Cause::Tick);
                }
                complete_operational_milestones(&mut rt, previous_phase, input);
            }
            if rt.engine.status.phase != Phase::Filling || !rt.ground_station_control {
                rt.fill_cutoff.reset();
                effects
            } else if let Some(mass) = rt.fresh_mass(input.now_ms) {
                if fill_stop_reached(&rt.engine.config, mass.value, target) {
                    match rt
                        .engine
                        .config
                        .pressure_limit(rt.engine.nitrous_target_psi, 50.0)
                        .and_then(|limit| {
                            gse_sequence::fill_trim::Controller::start(
                                input.now_ms,
                                target,
                                limit,
                                &rt.engine.config,
                            )
                        }) {
                        Ok((controller, mut effects)) => {
                            rt.engine.manual_override();
                            rt.engine.status.phase = Phase::PauseSetup;
                            rt.engine.claimed = true;
                            rt.engine.status.message =
                                "Closing supplies and vent before settled mass verification".into();
                            let reporting_trim = controller.progress_observation();
                            let reporting_view = rt.engine.progress_observation(input);
                            rt.progress.observe_trim(
                                reporting_trim,
                                reporting_view,
                                Some(mass),
                                input.now_ms,
                                None,
                                crate::sequence_progress_service::now_ms(),
                            );
                            rt.fill_trim = Some(controller);
                            effects.notification = Some("Automatic fill cutoff: closing valves, then settling and verifying final mass".into());
                            effects
                        }
                        Err(error) => pause_for_failure(&mut rt, input, &error),
                    }
                } else {
                    effects
                }
            } else {
                pause_for_failure(
                    &mut rt,
                    input,
                    "Automatic fill stopped: fresh calibrated fill weight is required",
                )
            }
        }
    };
    apply(state, effects);

}

/// The existing five-valve hardware cluster can be switched as one group.
pub fn panel_name(state: &AppState, name: &str) -> String {
    if !state.gse.lock().unwrap().engine.config.grouped_panel {
        return name.into();
    }
    match name {
        "Dump" => "CancelFill",
        "NormallyOpen" => "PauseFill",
        "Pilot" => "ValveSelfTest",
        "Nitrogen" => "NitrogenTest",
        "Nitrous" => "StartFill",
        other => other,
    }
    .into()
}
pub fn panel_command(state: &AppState, cmd: TelemetryCommand) -> TelemetryCommand {
    let name = panel_name(state, crate::sequences::command_name(&cmd));
    serde_json::from_value(serde_json::Value::String(name)).unwrap_or(cmd)
}
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .merge(progress::routes())
        .merge(cold_flow::routes())
        .route(
            "/api/gse/config",
            get(get_config).put(save_config).post(save_config),
        )
        .route("/api/gse/status", get(status))
        .route(
            "/api/gse/cold-flow/override",
            axum::routing::post(override_cold_flow),
        )
        .route("/api/gse/abort/clear", axum::routing::post(clear_abort))
        .route("/api/safety/about", get(crate::safety_catalog::about_route))
        .route(
            "/api/gse/overpressure/acknowledge",
            axum::routing::post(overpressure::acknowledge_route),
        )
        .route(
            "/api/gse/qualification/reset",
            axum::routing::post(reset_qualification),
        )
        .route(
            "/api/gse/self-test-confirmation",
            axum::routing::post(self_test_confirmation),
        )
}
#[derive(serde::Deserialize, serde::Serialize)]
struct SelfTestConfirmation {
    confirmed: bool,
}
async fn self_test_confirmation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<SelfTestConfirmation>,
) -> Response {
    let admission_generation = capture_operator_origin(&state).unwrap_or(0);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(e) => return e,
        };
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(_claim) = claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    {
        let mut rt = state.gse.lock().unwrap();
        if rt.engine.active() || (request.confirmed && rt.engine.status.self_test_locked) {
            return (StatusCode::CONFLICT,"Self-test confirmation is locked during a sequence or after nitrogen testing starts").into_response();
        }
        rt.engine.config.dry_self_test_confirmed = request.confirmed;
    }
    crate::sequences::refresh_action_policy_now(&state);
    Json(request).into_response()
}
async fn get_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    if crate::rrls::is_rrls() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Primary GSE configuration is not mirrored; use the primary ground station.",
        )
            .into_response();
    }
    Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
}
fn nitrogen_configuration_changed(before: &Config, after: &Config) -> bool {
    before.nitrogen_plan() != after.nitrogen_plan()
        || before.pressure_ceiling_psi != after.pressure_ceiling_psi
        || before.maximum_zero_offset_psi != after.maximum_zero_offset_psi
}
fn fill_configuration_changed(before: &Config, after: &Config) -> bool {
    before.turbocharge_pressure_deadband_psi != after.turbocharge_pressure_deadband_psi
        || before.turbocharge_max_added_mass_kg != after.turbocharge_max_added_mass_kg
        || before.fill_overfill_kg != after.fill_overfill_kg
        || before.fill_tolerance_kg != after.fill_tolerance_kg
        || before.fill_settle_ms != after.fill_settle_ms
}
fn invalidate_changed_configuration(rt: &mut Runtime, config: &Config) -> Result<(), String> {
    rt.turbo_evidence = None;
    let nitrogen = nitrogen_configuration_changed(&rt.engine.config, config);
    let fill = fill_configuration_changed(&rt.engine.config, config);
    if !nitrogen && !fill {
        return Ok(());
    }
    if !crate::runtime_mode::is_hitl() {
        if !rt.operational.saved.pending_checklists.is_empty() {
            return Err(
                "Save pending checklist progress before changing procedure settings".into(),
            );
        }
        let affected = if nitrogen {
            rt.operational.saved.qualification.nitrogen_satisfied()
                || rt
                    .operational
                    .saved
                    .qualification
                    .completed(Milestone::FillTarget)
        } else {
            rt.operational
                .saved
                .qualification
                .completed(Milestone::FillTarget)
        };
        if affected {
            if nitrogen {
                rt.operational.saved.qualification.invalidate_nitrogen()
            } else {
                rt.operational.saved.qualification.invalidate_fill()
            }
            .map_err(str::to_owned)?;
            rt.operational.saved.fill_target_kg = None;
            rt.operational.target_reached = None;
            let sequences: &[crate::checklists::Sequence] = if nitrogen {
                &[
                    crate::checklists::Sequence::NitrogenTest,
                    crate::checklists::Sequence::FillTarget,
                ]
            } else {
                &[crate::checklists::Sequence::FillTarget]
            };
            for sequence in sequences {
                rt.operational
                    .saved
                    .pending_checklists
                    .push(crate::checklists::CompletionEvent {
                        id: crate::checklists::paired::completion_event_id(),
                        sequence: *sequence,
                        at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                        action: crate::checklists::CompletionAction::Clear,
                    });
            }
            // Evidence is revoked durably before edited settings can be saved.
            // Any failure remains fail-closed and never dispatches outputs.
            rt.operational.persist()?;
            rt.operational.flush_checklists()?;
        }
    }
    if nitrogen {
        rt.engine.status.nitrogen_passed = false;
        rt.engine.status.nitrogen_overridden = false;
    }
    Ok(())
}
/// Overpressure settings an older client does not know are kept, never reset.
pub(super) const OVERPRESSURE_CONFIG_KEYS: [&str; 4] = [
    "overpressure_relief_psi",
    "overpressure_abort_psi",
    "overpressure_pilot_ms",
    "tank_rating_psi",
];
pub(super) fn config_from_request(current: &Config, mut body: serde_json::Value) -> Result<Config, String> {
    let object = body.as_object_mut().ok_or("GSE configuration must be an object")?;
    let saved = serde_json::to_value(current).map_err(|e| e.to_string())?;
    for key in OVERPRESSURE_CONFIG_KEYS {
        if !object.contains_key(key) {
            object.insert(key.into(), saved[key].clone());
        }
    }
    serde_json::from_value(body).map_err(|e| e.to_string())
}
async fn save_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let admission_generation = capture_operator_origin(&state).unwrap_or(0);
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await
    {
        return e;
    }
    let current = state.gse.lock().unwrap().engine.config.clone();
    let config = match config_from_request(&current, body) {
        Ok(config) => config,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, error).into_response(),
    };
    let Some(_claim) = claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if let Err(err) = config.validate_settings() {
        return (StatusCode::BAD_REQUEST, err).into_response();
    }
    let result = (|| -> Result<(), (StatusCode, String)> {
        let mut rt = state.gse.lock().unwrap();
        if rt.engine.active() || cold_flow::runtime_active(&rt) {
            return Err((
                StatusCode::CONFLICT,
                "Stop the active sequence before editing GSE configuration".into(),
            ));
        }
        // Bulk settings may preserve or revoke consent, but never grant it.
        // Compare and assign under this same lock so an intervening nitrogen
        // test cannot have its invalidated confirmation revived by stale UI.
        if config.dry_self_test_confirmed && !rt.engine.config.dry_self_test_confirmed {
            return Err((
                StatusCode::FORBIDDEN,
                "Grant dry self-test confirmation through its dedicated control".into(),
            ));
        }
        // Only an inert target-only edit with the COMPLETE original evidence
        // context still current bypasses the old invalidation behavior. This is
        // inactive settings work, never a sequence/report/control callback.
        let inert_cold = turbocharge::only_cold_target_changed(&rt.engine.config,&config)
            && turbocharge::evidence_context_current(&state,&rt);
        if !inert_cold {
            invalidate_changed_configuration(&mut rt, &config)
                .map_err(|error| (StatusCode::CONFLICT, error))?;
        }
        persist_settings(&config).map_err(|error| (StatusCode::CONFLICT, error))?;
        rt.engine.config = config;
        Ok(())
    })();
    match result {
        Ok(()) => {
            crate::sequences::refresh_action_policy_now(&state);
            Json(state.gse.lock().unwrap().engine.config.clone()).into_response()
        }
        Err(error) => error.into_response(),
    }
}
async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(e) = crate::web::authorize_headers(&state, &headers, Permission::ViewData).await {
        return e;
    }
    match status_value(&state) {
        Some(response) => Json(response).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Fresh primary GSE observations are unavailable; valve states are unknown.",
        )
            .into_response(),
    }
}

/// The `/api/gse/status` body. Every reply carries the sequence-progress
/// envelope: the UI treats a status without one as unavailable, so an RRLS
/// that left it out showed none of the primary's mirrored GSE state.
fn status_value(state: &Arc<AppState>) -> Option<serde_json::Value> {
    if crate::rrls::is_rrls() {
        return crate::primary_mirror::gse_status(state).map(|mut observation| {
            observation["sequence_progress"] = progress::envelope(state);
            observation
        });
    }
    let state = state.clone();
    let flight_state = *state.state.lock().unwrap();
    let prelaunch = prelaunch_flight_state(flight_state);
    let interlock = interlock(&state, &state.action_policy_snapshot());
    let status = {
        let rt = state.gse.lock().unwrap();
        let sampled_now = rt.now();
        rt.presentation_status_at(sampled_now, prelaunch, interlock)
    };
    let mut response = serde_json::to_value(status).expect("GSE status is serializable");
    let mode = crate::station_mode::snapshot();
    response["operating_mode"] = serde_json::to_value(&mode).expect("mode serializable");
    response["request_gate"] = serde_json::json!({
        "hitl_mode": crate::runtime_mode::is_hitl(),
        "flight_state": flight_state,
        "prelaunch": prelaunch,
        "button_interlock_satisfied": interlock,
    });
    response["dry_self_test_confirmed"] = serde_json::json!(
        state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed
    );
    response["configuration_error"] =
        serde_json::json!(state.gse.lock().unwrap().engine.config.validate().err());
    response["physical_panel"] = crate::gpio_panel::panel_indicators(&state);
    if !crate::runtime_mode::is_hitl() {
        let rt = state.gse.lock().unwrap();
        response["qualification"] =
            serde_json::to_value(&rt.operational.saved.qualification).unwrap();
        response["mission_stage"] =
            serde_json::to_value(rt.operational.saved.qualification.stage()).unwrap();
        response["topology_stable"] = serde_json::json!(rt.operational.topology_stable);
        response["storage_error"] = serde_json::json!(rt.operational.persist_error);
        response["automatic_abort_claimed"] =
            serde_json::json!(rt.operational.saved.automatic_abort_claimed);
        response["launch_queued"] = serde_json::json!(rt.operational.qd.queued());
        response["qd"] = serde_json::json!(format!("{:?}", rt.operational.qd.status(rt.now())));
    }
    if crate::runtime_mode::is_hitl() {
        let interlocked = self::interlock(&state, &state.action_policy_snapshot());
        let rt = state.gse.lock().unwrap();
        let blocker = hitl_nitrogen_override_blocker(&rt, interlocked, rt.now());
        response["nitrogen_override"] = serde_json::json!({
            "active": rt.engine.status.nitrogen_overridden,
            "available": blocker.is_none() && !rt.engine.status.nitrogen_overridden,
            "blocked_reason": blocker,
        });
    }
    turbocharge::decorate_status(&state, &mut response);
    let vb_connected = state
        .network_health
        .lock()
        .unwrap()
        .connected_for(crate::types::Board::ValveBoard.sender_id(), std::time::Instant::now())
        .is_some();
    overpressure::decorate_status(&state.gse.lock().unwrap(), vb_connected, &mut response);
    response["valve_reports"] = serde_json::to_value(crate::valve_report_health::status(&state))
        .expect("valve report health serializable");
    response["sequence_progress"]=progress::envelope(&state);
    response["cold_flow"]=serde_json::to_value(cold_flow::status(&state)).expect("Cold-flow status serializable");
    Some(response)
}

#[cfg(test)]
#[path = "gse/status_envelope_tests.rs"]
mod status_envelope_tests;

#[cfg(test)]
#[path = "gse/status_presentation_tests.rs"]
mod status_presentation_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn hitl_nitrogen_override_needs_interlock_closed_supplies_and_an_empty_tank() {
        use super::*;
        let mut rt = Runtime::default();
        rt.engine.config.maximum_zero_offset_psi = Some(8.0);
        let now = rt.now();
        rt.pressure = Some(Sample { value: 3.0, at_ms: now });
        let valve = |open| Some(ValveState { open, at_ms: now });
        rt.valves = [valve(false), valve(false), valve(false), valve(true), valve(false)];
        assert!(hitl_nitrogen_override_blocker(&rt, true, now).unwrap().contains("supply valves"));
        rt.valves[3] = valve(false);
        assert!(hitl_nitrogen_override_blocker(&rt, false, now).unwrap().contains("interlock"));
        rt.pressure = Some(Sample { value: 40.0, at_ms: now });
        assert!(hitl_nitrogen_override_blocker(&rt, true, now).unwrap().contains("empty-tank limit"));
        rt.pressure = Some(Sample { value: 3.0, at_ms: now });
        assert_eq!(hitl_nitrogen_override_blocker(&rt, true, now), None);
    }
    #[test]
    fn completion_valve_snapshot_retains_only_fresh_actual_open_and_closed() {
        use super::*;
        let input = Inputs {
            now_ms: 5000,
            pressure: None,
            valves: [
                Some(ValveState { open: true, at_ms: 4999 }),
                Some(ValveState { open: false, at_ms: 3000 }),
                None,
                Some(ValveState { open: false, at_ms: 2999 }),
                Some(ValveState { open: false, at_ms: 5001 }),
            ],
            prelaunch: false,
            interlock: false,
        };
        assert_eq!(completion_valve_snapshot(input), [Some(true), Some(false), None, None, None]);
    }
    #[test]
    fn template_switch_waits_for_local_reset_receipts() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut runtime = super::Runtime::default();
        assert!(runtime.checklist_definition_edit_allowed());
        runtime
            .operational
            .saved
            .pending_checklists
            .push(crate::checklists::CompletionEvent {
                id: "0".repeat(80),
                sequence: crate::checklists::Sequence::ValveSelfTest,
                at_ms: 0,
                action: crate::checklists::CompletionAction::Clear,
            });
        assert!(!runtime.checklist_definition_edit_allowed());
        runtime.operational.saved.pending_checklists.clear();
        assert!(runtime.checklist_definition_edit_allowed());
    }
    #[test]
    fn committed_fire_pilot_clock_advances_a_paused_hold_but_qualification_does_not() {
        let mut engine = Engine::default();
        engine.status.phase = Phase::Paused;
        assert!(
            engine.active(),
            "The filled-tank hold still owns GSE controls"
        );
        assert!(!pilot_clock_allowed(engine.active(), false, false));
        assert!(pilot_clock_allowed(engine.active(), true, false));
        // The former !Engine::active guard rejected this genuine Fire case.
        let old_guard_allowed = !engine.active();
        assert!(!old_guard_allowed);
    }
    #[tokio::test]
    async fn hitl_pilot_clock_wrapper_keeps_the_original_active_engine_guard() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().engine.status.phase = Phase::Paused;
        assert!(!pilot_report_can_advance_launch_clock(&state));
        state.gse.lock().unwrap().engine.status.phase = Phase::Idle;
        assert!(pilot_report_can_advance_launch_clock(&state));
    }
    #[test]
    fn pilot_clock_guard_preserves_hitl_and_inactive_behavior() {
        for fire in [false, true] {
            assert!(!pilot_clock_allowed(true, fire, true));
            assert!(pilot_clock_allowed(false, fire, true));
            assert!(pilot_clock_allowed(false, fire, false));
        }
    }
    #[test]
    fn quantized_automatic_target_cutoff_keeps_the_existing_qualification_tolerance() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut runtime = Runtime::default();
        let target = 5.0;
        let quantized_mass = target * 0.996;
        assert!(quantized_mass < target);
        assert!(runtime.fill_cutoff.target_met(quantized_mass, target));
        remember_automatic_cutoff_target(&mut runtime, quantized_mass, target);
        assert_eq!(runtime.operational.target_reached, Some(target));
        assert!(
            !runtime
                .operational
                .saved
                .qualification
                .completed(Milestone::FillTarget),
            "The candidate alone must not grant qualification before fresh mass and valve ACKs"
        );
    }
    #[test]
    fn below_target_plateau_and_invalid_inputs_do_not_record_a_fill_target() {
        crate::mode_suite!(Normal, TestFire, Testing);
        for (mass, target) in [
            (4.9, 5.0),
            (f32::NAN, 5.0),
            (f32::INFINITY, 5.0),
            (5.0, f32::NAN),
            (5.0, f32::INFINITY),
            (0.0, 0.0),
            (-4.0, -5.0),
        ] {
            let mut runtime = Runtime::default();
            remember_automatic_cutoff_target(&mut runtime, mass, target);
            assert_eq!(runtime.operational.target_reached, None, "{mass}/{target}");
        }
    }
    #[test]
    fn precision_inputs_keep_source_age_and_replays_cannot_create_settled_samples() {
        let utc = 1_798_000_000_000;
        let mut previous = None;
        assert_eq!(
            source_sample_at(Some(utc - 1500), utc, 5000, &mut previous),
            Some(3500)
        );
        assert_eq!(
            source_sample_at(Some(utc - 1500), utc + 100, 5100, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc - 3000), utc, 5000, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc + 101), utc, 5000, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(utc - 100), utc, 5000, &mut previous),
            Some(4900)
        );
        assert_eq!(
            source_sample_at(Some(utc - 200), utc, 5000, &mut previous),
            None
        );
        let mut previous = None;
        assert_eq!(
            source_sample_at(Some(1234), utc, 5000, &mut previous),
            Some(5000)
        );
        assert_eq!(
            source_sample_at(Some(1234), utc + 100, 5100, &mut previous),
            None
        );
        assert_eq!(
            source_sample_at(Some(1235), utc + 100, 5100, &mut previous),
            Some(5100)
        );
    }
    #[tokio::test]
    async fn replayed_valve_capture_does_not_acknowledge_a_new_target() {
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().clock = Instant::now() - std::time::Duration::from_secs(5);
        let utc = 1_798_000_000_000;
        assert!(state.set_umbilical_valve_state_source(0, false, utc - 1000, utc, 11));
        let before = state.gse.lock().unwrap().valves[0].unwrap().at_ms;
        state.set_pending_umbilical_valve_state(0, false);
        assert!(!state.set_umbilical_valve_state_source(0, false, utc - 1000, utc + 100, 11));
        assert_eq!(state.gse.lock().unwrap().valves[0].unwrap().at_ms, before);
        assert_eq!(state.get_pending_umbilical_valve_state(0), Some(false));
        assert!(!state.set_umbilical_valve_state_source(0, true, utc - 3000, utc, 22));
        assert!(state.set_umbilical_valve_state_source(0, false, utc + 1, utc + 1, 33));
    }
    #[test]
    fn precision_band_and_overshoot_are_absolute_and_signed_not_percentage() {
        let config = Config::default();
        for sign in [1.0, -1.0] {
            assert!(fill_mass_in_band(&config, 100.06 * sign, 100.0 * sign));
            assert!(!fill_mass_in_band(&config, 99.6 * sign, 100.0 * sign));
            assert!(!fill_stop_reached(&config, 100.0 * sign, 100.0 * sign));
            assert!(fill_stop_reached(&config, 100.06 * sign, 100.0 * sign));
        }
    }
    #[test]
    fn sequence_clock_uses_only_explicit_simulation_time() {
        assert_eq!(super::sequence_clock_ms(25_000, Some(1_000)), 1_000);
        assert_eq!(super::sequence_clock_ms(26_000, Some(1_000)), 1_000);
        assert_eq!(super::sequence_clock_ms(27_000, Some(2_000)), 2_000);
        assert_eq!(super::sequence_clock_ms(25_000, None), 25_000);
    }

    use super::*;

    #[tokio::test]
    async fn clear_abort_rejects_non_aborted_and_active_sequences() {
        let state = crate::state::tests::test_app_state().await;
        for phase in [
            FlightState::Idle,
            FlightState::NitrousFill,
            FlightState::Launch,
            FlightState::Ascent,
        ] {
            *state.state.lock().unwrap() = phase;
            assert_eq!(
                clear_ground_abort(&state).unwrap_err().0,
                StatusCode::CONFLICT
            );
            assert_eq!(state.local_flight_state_snapshot(), phase);
        }
        *state.state.lock().unwrap() = FlightState::Aborted;
        state.gse.lock().unwrap().engine.status.phase = Phase::BaselineSetup;
        assert_eq!(
            clear_ground_abort(&state).unwrap_err().0,
            StatusCode::CONFLICT
        );
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
    }

    #[tokio::test]
    async fn clear_abort_publishes_idle_without_output_commands() {
        let (state, sent, peer) = button_test_state().await;
        *state.state.lock().unwrap() = FlightState::Aborted;
        crate::network_variables::set_flight_state(
            state.topology_router.get().unwrap(),
            FlightState::Aborted as u8,
        )
        .unwrap();
        let clock = state.launch_clock_snapshot();
        let hitl = crate::runtime_mode::is_hitl();
        let path = std::env::temp_dir().join(format!("gs-clear-abort-{}.json", std::process::id()));
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.fault(0, "Operator abort");
            rt.engine.config.dry_self_test_confirmed = true;
            if !hitl {
                rt.operational.path = Some(path.clone());
                rt.operational
                    .saved
                    .qualification
                    .latch(Fault::TopologyLost);
            }
        }
        clear_ground_abort(&state).unwrap();
        for _ in 0..10 {
            tick(&state, true);
        }
        pump_test_link(&state, &peer);
        let cached = state
            .topology_router
            .get()
            .unwrap()
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .expect("reconnecting nodes need a replacement managed value");
        assert_eq!(cached.payload(), &[FlightState::Idle as u8]);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        // The persisted default store is process-global and other parallel
        // tests legitimately update it through their independent AppStates.
        // The router cache above is this station's reconnection contract.
        assert_eq!(state.launch_clock_snapshot(), clock);
        assert!(
            sent.lock().unwrap().is_empty(),
            "clearing must not actuate outputs"
        );
        let rt = state.gse.lock().unwrap();
        assert_eq!(rt.engine.status.phase, Phase::Idle);
        assert!(!rt.engine.config.dry_self_test_confirmed);
        if !hitl {
            let saved = operational::Saved::load(&path).unwrap();
            assert!(saved.qualification.fault().is_none());
            assert!(!saved.qualification.completed(Milestone::SelfTest));
            assert_eq!(saved.qualification.generation(), 1);
            assert!(rt.operational.startup_configured);
            std::fs::remove_file(path).unwrap();
        }
    }

    /// A held SDK core is not a pending Abort: Clear GSE abort waits out the
    /// router pass instead of refusing the operator.
    #[tokio::test]
    async fn clear_abort_waits_out_a_held_sdk_core() {
        crate::mode_suite!(Normal);
        let state = crate::state::tests::test_app_state().await;
        let (router, core) = crate::core_busy::hold::router(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        );
        crate::telemetry_task::refresh_host_network_time(
            &router,
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        state.topology_router.set(router).unwrap();
        *state.state.lock().unwrap() = FlightState::Aborted;
        let path = std::env::temp_dir().join(format!("gs-clear-abort-busy-{}.json", rand::random::<u64>()));
        state.gse.lock().unwrap().operational.path = Some(path.clone());
        let held = core.hold();
        assert_eq!(
            clear_ground_abort(&state),
            Err((StatusCode::SERVICE_UNAVAILABLE, ROUTER_CORE_BUSY.to_string())),
            "a held core decides nothing; it is never a pending Abort"
        );
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
        let release = held.release_after(std::time::Duration::from_millis(60));
        clear_ground_abort_waiting_out_core(&state).await.unwrap();
        release.join().unwrap();
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn clear_abort_without_network_time_preserves_latched_state() {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Aborted);
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        ));
        state.topology_router.set(router).unwrap();
        let hitl = crate::runtime_mode::is_hitl();
        let saved = (!hitl)
            .then(|| serde_json::to_value(&state.gse.lock().unwrap().operational.saved).unwrap());
        let error = clear_ground_abort(&state).unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(error.1.contains("synchronized network time"));
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Aborted);
        if let Some(saved) = saved {
            assert_eq!(
                serde_json::to_value(&state.gse.lock().unwrap().operational.saved).unwrap(),
                saved
            );
        }
    }

    pub(super) type TestCommands = Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    pub(super) async fn button_test_state()
    -> (Arc<AppState>, TestCommands, Arc<sedsnet::router::Router>) {
        button_test_state_observed(None).await
    }
    pub(super) async fn button_test_state_observed(
        observer: Option<FillRecordingTestObserver>,
    ) -> (Arc<AppState>, TestCommands, Arc<sedsnet::router::Router>) {
        button_test_state_with(observer, |config| Arc::new(sedsnet::router::Router::new(config))).await
    }
    /// The same station, with a host router whose SDK core a test can hold.
    pub(super) async fn button_test_state_holdable()
    -> (Arc<AppState>, TestCommands, Arc<sedsnet::router::Router>, crate::core_busy::hold::Holdable) {
        let mut core = None;
        let (state, sent, peer) = button_test_state_with(None, |config| {
            let (router, holdable) = crate::core_busy::hold::router(config);
            core = Some(holdable);
            router
        })
        .await;
        (state, sent, peer, core.unwrap())
    }
    async fn button_test_state_with(
        observer: Option<FillRecordingTestObserver>,
        make_router: impl FnOnce(sedsnet::router::RouterConfig) -> Arc<sedsnet::router::Router>,
    ) -> (Arc<AppState>, TestCommands, Arc<sedsnet::router::Router>) {
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = FlightState::Idle;
        let sent: TestCommands = Default::default();
        let endpoints = [
            ("VALVE_BOARD", "VALVE_COMMAND"),
            ("ACTUATOR_BOARD", "ACTUATOR_COMMAND"),
        ]
        .map(|(endpoint, name)| {
            let sent = sent.clone();
            let observer = observer.clone();
            sedsnet::router::EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint(endpoint),
                move |packet: &sedsnet::packet::Packet| {
                    sent.lock()
                        .unwrap()
                        .push((name.into(), packet.payload().to_vec()));
                    if let Some(observer) = &observer {
                        observer(packet);
                    }
                    Ok(())
                },
            )
        });
        let peer = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new(endpoints).with_sender("GSE_TEST"),
        ));
        let router = make_router(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        crate::telemetry_task::refresh_host_network_time(
            &router,
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        let ground_weak = Arc::downgrade(&router);
        let ground_ingress = Arc::new(std::sync::OnceLock::new());
        let ingress = ground_ingress.clone();
        let options = sedsnet::router::RouterSideOptions {
            // Match the deployed CAN/I2C and RF sides: no hop ACKs;
            // ordered end-to-end command ACKs still drive this test link.
            reliable_enabled: false,
            link_local_enabled: true,
            ..Default::default()
        }
        .with_small_packet_transport(128);
        let peer_side = peer.add_side_packed_with_options(
            "test_ground",
            move |bytes| {
                ground_weak
                    .upgrade()
                    .unwrap()
                    .rx_packed_from_side(bytes, *ingress.get().unwrap())
            },
            options.clone(),
        );
        let receiver = peer.clone();
        let ground_side = router.add_side_packed_with_options(
            "test_fill",
            move |bytes| receiver.rx_packed_from_side(bytes, peer_side),
            options,
        );
        ground_ingress.set(ground_side).unwrap();
        peer.announce_discovery().unwrap();
        peer.process_all_queues_with_timeout(0).unwrap();
        state.topology_router.set(router).unwrap();
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.config.maximum_zero_offset_psi = Some(8.0);
            rt.observe_pressure(Some(0.0));
            rt.observe_mass(Some(0.0));
        }
        (state, sent, peer)
    }

    pub(super) fn pump_test_link(state: &AppState, peer: &sedsnet::router::Router) {
        // Ordered command delivery requires the receiver's protocol ACK queue
        // to be serviced too, just as the board's telemetry worker does.
        for _ in 0..20 {
            peer.process_all_queues_with_timeout(0).unwrap();
            state
                .topology_router
                .get()
                .unwrap()
                .process_all_queues_with_timeout(0)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn manual_handoff_fences_prepared_effects_before_the_requested_command() {
        let (state, sent, peer) = button_test_state().await;
        let prepared = dispatch_gate();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let manual_state = state.clone();
        let manual = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let handled = handle_command(&manual_state, &TelemetryCommand::NitrogenClose);
            finished_tx.send(handled).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            finished_rx.try_recv().is_err(),
            "manual cancellation must wait for prepared admission"
        );
        apply(
            &state,
            Effects {
                valves: vec![(3, true)],
                notification: None,
            },
        );
        drop(prepared);
        assert!(
            !finished_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
        );
        manual.join().unwrap();
        // Model the existing outer command dispatch after handle_command(false).
        crate::telemetry_task::queue_controller_command(
            state.topology_router.get().unwrap(),
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[12],
        )
        .unwrap();
        pump_test_link(&state, &peer);
        let before = sent.lock().unwrap().clone();
        assert_eq!(
            before,
            vec![
                ("ACTUATOR_COMMAND".into(), vec![9 | 0x80]),
                ("ACTUATOR_COMMAND".into(), vec![12])
            ]
        );
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert_eq!(
            *sent.lock().unwrap(),
            before,
            "cancelled ACK queues and startup must stay inert"
        );
    }

    pub(super) fn confirm_test_valves(state: &AppState, valves: [bool; 5]) {
        let mut rt = state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip(valves) {
            rt.observe_valve(key, open);
        }
        rt.observe_pressure(Some(0.0));
        rt.observe_mass(Some(0.0));
    }

    fn drive_button_phase(
        state: &Arc<AppState>,
        sent: &TestCommands,
        peer: &sedsnet::router::Router,
        done: Phase,
        mass: f32,
    ) {
        let mut valves = gse_sequence::CLOSED;
        let mut consumed = 0;
        for _ in 0..200 {
            pump_test_link(state, peer);
            {
                let commands = sent.lock().unwrap();
                for (kind, bytes) in &commands[consumed..] {
                    assert_eq!(bytes.len(), 1);
                    assert_ne!(
                        bytes[0] & 0x80,
                        0,
                        "automatic provenance must survive the wire"
                    );
                    let command = bytes[0] & 0x7f;
                    let indices = if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 };
                    let i = indices
                        .into_iter()
                        .find(|i| command == OPEN[*i] || command == CLOSE[*i])
                        .unwrap();
                    valves[i] = command == OPEN[i];
                }
                consumed = commands.len();
            }
            {
                let mut rt = state.gse.lock().unwrap();
                let next = rt.now() + 100;
                rt.clock = Instant::now() - std::time::Duration::from_millis(next);
                for (key, open) in KEYS.into_iter().zip(valves) {
                    rt.observe_valve(key, open);
                }
                rt.observe_pressure(Some(0.));
                rt.observe_mass(Some(mass));
            }
            tick(state, true);
            let rt = state.gse.lock().unwrap();
            assert_ne!(
                rt.engine.status.phase,
                Phase::Fault,
                "{}",
                rt.engine.status.message
            );
            if rt.engine.status.phase == done && (done != Phase::Filling || valves[4]) {
                return;
            }
        }
        panic!("routed sequence did not reach {done:?}");
    }
    #[tokio::test]
    async fn nitrogen_button_enters_baseline_and_requests_safe_valve_configuration() {
        crate::mode_suite!(Hitl);
        let (state, sent, peer) = button_test_state().await;
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::NitrogenTest),
            Some(true)
        );
        assert!(handle_command(&state, &TelemetryCommand::NitrogenTest));
        pump_test_link(&state, &peer);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![("ACTUATOR_COMMAND".into(), vec![12 | 0x80])],
            "serial close admission must precede later targets"
        );
        drive_button_phase(&state, &sent, &peer, Phase::Baseline, 0.);
        assert_eq!(
            sent.lock().unwrap().len(),
            5,
            "all physical targets require reports"
        );
    }
    #[tokio::test]
    async fn fill_button_requires_nitrogen_pass_then_opens_only_after_confirmations_and_stops_at_mass()
     {
        crate::mode_suite!(Hitl);
        let (state, sent, peer) = button_test_state().await;
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        assert!(sent.lock().unwrap().is_empty());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.status.phase = Phase::Passed;
            rt.engine.status.nitrogen_passed = true;
            rt.engine.status.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.,
                min_psi: 0.,
                max_psi: 0.,
                noise_psi: 0.1,
                samples: 50,
            });
        }
        assert!(handle_command(&state, &TelemetryCommand::StartFill));
        drive_button_phase(&state, &sent, &peer, Phase::Filling, 0.);
        pump_test_link(&state, &peer);
        assert!(
            sent.lock()
                .unwrap()
                .contains(&("ACTUATOR_COMMAND".into(), vec![10 | 0x80]))
        );
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        {
            let mut rt = state.gse.lock().unwrap();
            let stop = target + target.signum() * rt.engine.config.fill_overfill_kg;
            rt.observe_mass(Some(stop));
        }
        tick(&state, true);
        assert!(state.gse.lock().unwrap().fill_trim.is_some());
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::PauseSetup
        );
        drive_button_phase(&state, &sent, &peer, Phase::Paused, target);
        assert!(state.gse.lock().unwrap().fill_trim.is_none());
        {
            let rt = state.gse.lock().unwrap();
            let now = rt.now();
            assert!(rt.valves.iter().all(|value| value.is_some_and(|value|
                !value.open && value.at_ms <= now && now - value.at_ms <= 2000)),
                "trim completion requires all five fresh actual closed reports: {:?}", rt.valves);
        }
        let completed_status = state.gse.lock().unwrap().engine.status.clone();
        let completed_snapshot = serde_json::to_value(completed_status).unwrap();
        assert_eq!(completed_snapshot["valves"], serde_json::json!([false, false, false, false, false]),
            "completion snapshot must immediately reflect actual closed reports");
        let completed_commands = sent.lock().unwrap().clone();
        {
            let mut rt = state.gse.lock().unwrap();
            rt.observe_mass(Some(target - target.signum() * 0.3));
        }
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Paused);
        assert_eq!(*sent.lock().unwrap(), completed_commands, "weight loss cannot auto-resume fill");
    }

    #[tokio::test]
    async fn disabling_mass_control_cancels_trim_without_changing_valves() {
        crate::mode_suite!(Hitl);
        let (state, sent, peer) = button_test_state().await;
        {
            let mut rt = state.gse.lock().unwrap();
            let (controller, _) =
                gse_sequence::fill_trim::Controller::start(rt.now(), 5., 200., &rt.engine.config)
                    .unwrap();
            rt.fill_trim = Some(controller);
            rt.engine.status.phase = Phase::PauseSetup;
        }
        assert!(handle_command(
            &state,
            &TelemetryCommand::ToggleGroundStationControl
        ));
        {
            let rt = state.gse.lock().unwrap();
            assert!(!rt.ground_station_control);
            assert!(rt.fill_trim.is_none());
            assert_eq!(rt.engine.status.phase, Phase::Idle);
            assert!(rt.manual_control);
        }
        tick(&state, true);
        pump_test_link(&state, &peer);
        assert!(
            sent.lock().unwrap().is_empty(),
            "turning off must not retain vent pulses or force other outputs"
        );
    }
    #[tokio::test]
    async fn button_rejections_are_visible_and_do_not_enqueue_valve_actions() {
        crate::mode_suite!(Hitl);
        for cmd in [TelemetryCommand::NitrogenTest, TelemetryCommand::StartFill] {
            let (state, sent, peer) = button_test_state().await;
            state
                .gse
                .lock()
                .unwrap()
                .engine
                .config
                .maximum_zero_offset_psi = None;
            assert!(handle_command(&state, &cmd));
            pump_test_link(&state, &peer);
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Idle);
            assert!(rt.engine.status.message.contains("empty-tank PT offset"));
            drop(rt);
            for key in KEYS {
                assert!(state.get_pending_umbilical_valve_state(key).is_none());
            }
            assert!(sent.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn fill_nitrogen_and_self_test_commands_remain_registered_in_every_mode() {
        for (name, command, expected) in [
            ("StartFill", TelemetryCommand::StartFill, Action::StartFill),
            ("PauseFill", TelemetryCommand::PauseFill, Action::PauseFill),
            (
                "CancelFill",
                TelemetryCommand::CancelFill,
                Action::CancelFill,
            ),
            (
                "NitrogenTest",
                TelemetryCommand::NitrogenTest,
                Action::NitrogenTest,
            ),
            (
                "ValveSelfTest",
                TelemetryCommand::ValveSelfTest,
                Action::SelfTest,
            ),
        ] {
            assert!(ACTIONS.contains(&(name, expected)));
            assert_eq!(action(&command), Some(expected));
        }
    }

    #[test]
    fn hitl_covers_all_prelaunch_states_but_never_flight_or_recovery() {
        for state in [
            FlightState::Idle,
            FlightState::PreFill,
            FlightState::FillTest,
            FlightState::NitrogenFill,
            FlightState::NitrousFill,
        ] {
            assert!(prelaunch_flight_state(state));
        }
        assert!(prelaunch_flight_state(FlightState::Armed));
        assert_eq!(
            prelaunch_flight_state(FlightState::Startup),
            crate::runtime_mode::is_hitl()
        );
        for state in [
            FlightState::Launch,
            FlightState::Ascent,
            FlightState::Coast,
            FlightState::Apogee,
            FlightState::Descent,
            FlightState::Reefing,
            FlightState::Landed,
            FlightState::Recovery,
            FlightState::Aborted,
        ] {
            assert!(!prelaunch_flight_state(state));
        }
        if crate::runtime_mode::is_hitl() {
            for (_, action) in ACTIONS {
            let rt = Runtime::default();
            assert!(!sequence_button_enabled(
                &rt.engine,
                action,
                rt.input(false, true)
            ));
            }
        }
    }

    #[test]
    fn automatic_fill_defaults_on_and_rejects_stale_or_invalid_weight() {
        let mut rt = Runtime::default();
        assert!(rt.ground_station_control);
        assert!(rt.fresh_mass(0).is_none());
        rt.observe_mass(Some(f32::NAN));
        assert!(rt.mass.is_none());
        rt.mass = Some(Sample {
            value: 10.0,
            at_ms: 100,
        });
        assert!(rt.fresh_mass(2100).is_some());
        assert!(rt.fresh_mass(2101).is_none());
        assert!(rt.fresh_mass(99).is_none());
    }

    #[test]
    fn button_availability_does_not_bypass_sequence_safety() {
        let mut rt = Runtime::default();
        let input = rt.input(true, true);
        for (_, action) in ACTIONS {
            if crate::runtime_mode::is_hitl() {
                assert_eq!(
                    sequence_button_enabled(&rt.engine, action, input),
                    action != Action::SelfTest
                );
            } else {
                assert_eq!(
                    sequence_button_enabled(&rt.engine, action, input),
                    rt.engine.allows(action, input)
                );
            }
        }
        let before = rt.engine.status.phase;
        assert!(
            rt.engine
                .request(Action::NitrogenTest, input)
                .unwrap_err()
                .contains("empty-tank PT offset")
        );
        assert_eq!(rt.engine.status.phase, before);
        rt.engine.config.dry_self_test_confirmed = true;
        if crate::runtime_mode::is_hitl() {
            assert!(sequence_button_enabled(&rt.engine, Action::SelfTest, input));
            rt.engine.status.self_test_locked = true;
            assert!(!sequence_button_enabled(
                &rt.engine,
                Action::SelfTest,
                input
            ));
            for (_, action) in ACTIONS {
                assert!(!sequence_button_enabled(
                    &rt.engine,
                    action,
                    rt.input(true, false)
                ));
            }
        }
    }
}

/// Status receipt is checked before the next sequence tick; never wait five
/// seconds to react to an incompatible supply opening during nitrous fill.
pub fn report_safety(state: &Arc<AppState>) {
    // Its typed controller checks fresh PT/outputs on the existing GSE tick.
    // Paused here is a proxy, never proof of a nitrous-fill session.
    if cold_flow::active(state){return;}
    if !crate::runtime_mode::is_hitl() {
        let _claim = {
            let _dispatch = dispatch_gate();
            let Some(claim) =
                claim_operator_dispatch_under_gate(state, automatic_generation(state))
            else {
                return;
            };
            check_report_safety(state);
            claim
        };
        flush_effects(state);
    progress::flush(state);
    }
}
pub fn qd_confirmation(state: &AppState, sender: &str, key: u8, open: bool, source_ms: u64) {
    if !crate::runtime_mode::is_hitl()
        && sender == crate::types::Board::ActuatorBoard.sender_id()
        && key == crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8
        && open
    {
        let mut rt = state.gse.lock().unwrap();
        if rt
            .operational
            .qd_dispatch_utc_ms
            .is_some_and(|sent| source_ms > sent)
        {
            let now = rt.now();
            rt.operational.qd.confirm(now);
        }
    }
}
/// RRLS relays telemetry and shared settings but cannot change live operation.
fn require_primary_operational_station() -> Result<(), (StatusCode, String)> {
    if crate::rrls::is_rrls() {
        Err((
            StatusCode::FORBIDDEN,
            "Live GSE operations are read-only on RRLS".into(),
        ))
    } else {
        Ok(())
    }
}
/// A sequence holding Paused has confirmed its valves closed and is waiting for
/// the operator, which is where a fill sits through a test fire. The reset is
/// the operator ending it, so only a sequence still moving valves blocks it.
fn engine_blocks_operator_reset(rt: &Runtime) -> bool {
    rt.engine.active() && rt.engine.status.phase != Phase::Paused
}
pub(crate) fn verified_recovery_host_idle(state: &AppState) -> bool {
    let rt = state.gse.lock().unwrap();
    if rt.engine.active() {
        return false;
    }
    if !crate::runtime_mode::is_hitl() && (rt.operational.running || rt.automatic_abort_in_flight) {
        return false;
    }
    true
}
/// Explicit reset may retire host Launch metadata only after host work has
/// drained. Board completion is still verified by the recovery transaction. A
/// Launch waiting on its Valve Board, or the latch left by an earlier Stop, is
/// not work: this reset is the way out of both.
/// Plain-language reason `try_operator_reset_host_idle` is not Ok(true), for the operator.
/// Host work that blocks a reset or resync, from RAM owners only: never waits
/// on the network router, so a recovery in flight can recheck it.
pub(crate) fn operator_work_block_reason(state: &AppState) -> Option<String> {
    {
        let rt = state.gse.lock().unwrap();
        if engine_blocks_operator_reset(&rt) {
            return Some("A GSE sequence is running; pause or cancel it first.".into());
        }
        if cold_flow::runtime_active(&rt) {
            return Some("Cold flow is running; stop it first.".into());
        }
        if !crate::runtime_mode::is_hitl() && (rt.operational.running || rt.automatic_abort_in_flight) {
            return Some("An operation or automatic abort is still running.".into());
        }
    }
    if state.manual_abort_intent.pending_epoch().is_some() || state.deferred_abort.manual_pending() {
        return Some("A manual Abort is still being delivered.".into());
    }
    None
}
pub(crate) fn operator_reset_block_reason(state: &AppState) -> Option<String> {
    if let Some(reason) = operator_work_block_reason(state) {
        return Some(reason);
    }
    let Some(router) = state.topology_router.get() else {
        return Some("The hardware network router is not running.".into());
    };
    for name in ["ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND", "FLIGHT_COMMAND"] {
        match crate::abort_retirement::try_has_pending_reliable_tx_type(router, crate::telemetry_schema::data_type(name)) {
            Ok(false) => {}
            Ok(true) => return Some(format!("A {name} is still waiting for a board acknowledgement; wait for it to finish or time out.")),
            Err(_) => return Some(ROUTER_CORE_BUSY.into()),
        }
    }
    None
}
/// A latched tank overpressure, at the station or freshly reported by the Valve
/// Board, is an active safety condition: a board Abort may still be protecting
/// the tank. Resync never clears an Abort while this holds.
// Safety catalog (About safety): board-resync. Keep backend/src/safety_catalog_rules.rs in step.
#[cfg(test)]
pub(crate) fn set_overpressure_latch_for_test(state: &AppState, latch: Option<overpressure::Latch>) {
    state.gse.lock().unwrap().overpressure.latch = latch;
}
pub(crate) fn overpressure_holds_abort(state: &AppState) -> bool {
    let rt = state.gse.lock().unwrap();
    let now = rt.now();
    rt.overpressure.latched() || rt.overpressure.board_owns(now)
}
/// Never waits behind the SDK owner. Err only while another thread holds the
/// SDK core, so nothing was decided; never a manual-command veto.
pub(crate) fn try_operator_reset_host_idle(
    state: &AppState,
) -> Result<bool, sedsnet::router::TryPendingTypeError> {
    if !operator_reset_host_idle_ram(state) {
        return Ok(false);
    }
    let Some(router) = state.topology_router.get() else { return Ok(false); };
    for name in ["ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND", "FLIGHT_COMMAND"] {
        if crate::abort_retirement::try_has_pending_reliable_tx_type(
            router, crate::telemetry_schema::data_type(name))? {
            return Ok(false);
        }
    }
    Ok(true)
}
/// Host work owners only (RAM), without the router's in-flight commands.
pub(crate) fn operator_reset_host_idle_ram(state: &AppState) -> bool {
    {
        let rt = state.gse.lock().unwrap();
        if engine_blocks_operator_reset(&rt) || cold_flow::runtime_active(&rt) {
            return false;
        }
        if !crate::runtime_mode::is_hitl() && (rt.operational.running || rt.automatic_abort_in_flight) {
            return false;
        }
    }
    !(state.manual_abort_intent.pending_epoch().is_some() || state.deferred_abort.manual_pending())
}
/// Called only by the closed verified Reset ALL owner after all seven boards
/// applied Startup. This does not publish state, reopen, or dispatch outputs.
pub(crate) fn prepare_verified_startup(state: &Arc<AppState>) -> Result<(), &'static str> {
    let mut rt = state.gse.lock().unwrap();
    if engine_blocks_operator_reset(&rt) {
        return Err("Sequence still active");
    }
    if !crate::runtime_mode::is_hitl() {
        if rt.operational.running || rt.automatic_abort_in_flight {
            return Err("GSE owner still active");
        }
        rt.operational
            .saved
            .qualification
            .reset()
            .map_err(|_| "Qualification reset refused")?;
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.saved.turbocharge = None;
        rt.operational.saved.turbocharge_binding = None;
        rt.operational.saved.pending_checklists.clear();
        rt.operational.saved.checklist_digest = None;
        rt.operational.saved.automatic_abort_claimed = false;
        rt.operational
            .persist()
            .map_err(|_| "Qualification persistence failed")?;
        crate::checklists::reset_sequence_completions()
            .map_err(|_| "Checklist reset persistence failed")?;
        rt.operational.qd = QdWait::default();
        rt.operational.qd_launch_admission = None;
        rt.operational.qd_dispatch_utc_ms = None;
        rt.operational.startup_configured = true;
    }
    let config = rt.engine.config.clone();
    rt.engine = station_engine();
    rt.engine.config = config;
    rt.engine.config.dry_self_test_confirmed = false;
    rt.manual_control = false;
    rt.fill_trim = None;
    Ok(())
}
/// A reliable command frame outstanding at least this long (from when the
/// station first saw it pending) counts as undeliverable rather than still in
/// delivery. One shared rule for Clear GSE abort and the station mode switch.
pub(crate) const UNDELIVERABLE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
// First-seen times only: still valid if a panic interrupts an update, so a
// poisoned lock is taken over instead of failing Clear GSE abort.
static DELIVERY_FIRST_SEEN: std::sync::Mutex<Option<std::collections::HashMap<(usize, u64), std::time::Instant>>> = std::sync::Mutex::new(None);
/// The pending reliable frames of these types that are still in their normal
/// delivery window: outstanding less than UNDELIVERABLE_AFTER, judged both by
/// when the station first saw them pending and by their retry count, and not
/// waiting behind a predecessor. Returns their packet ids.
#[cfg(test)]
pub(crate) fn commands_still_in_delivery(router: &sedsnet::router::Router, kinds: &[sedsnet::config::DataType]) -> Vec<u64> {
    // A held SDK core counts as still queued: never report delivery done.
    delivery_ids(router, kinds, true).unwrap_or_default()
}
/// Same as [`commands_still_in_delivery`], except that a held SDK core is
/// reported as such instead of being counted as a queued command.
pub(crate) fn try_commands_still_in_delivery(
    router: &sedsnet::router::Router,
    kinds: &[sedsnet::config::DataType],
) -> Result<Vec<u64>, sedsnet::router::TryPendingTypeError> {
    delivery_ids(router, kinds, false)
}
fn delivery_ids(
    router: &sedsnet::router::Router,
    kinds: &[sedsnet::config::DataType],
    busy_is_queued: bool,
) -> Result<Vec<u64>, sedsnet::router::TryPendingTypeError> {
    // Sample the nonwaiting pending check first, so a held core is reported
    // before the stats export below waits for that core.
    let mut pending = Vec::with_capacity(kinds.len());
    for kind in kinds {
        pending.push(match router.try_has_pending_reliable_tx_type(*kind) {
            Ok(pending) => pending,
            Err(_) if busy_is_queued => true,
            Err(busy) => return Err(busy),
        });
    }
    let rows = router.export_reliable_pending_stats();
    let now = std::time::Instant::now();
    let window = crate::sim_freshness::wall(UNDELIVERABLE_AFTER);
    let retry_ms = u64::from(sedsnet::config::runtime_tuning_config().reliable_retransmit_ms.max(1));
    // Pending in the SDK but not yet a tracked delivery (still queued, or no
    // advertised destination yet): tracked per type under a synthetic key.
    let queued = kinds.iter().zip(pending)
        .filter(|(kind, pending)| *pending && !rows.iter().any(|row| row.data_type == **kind))
        .map(|(kind, _)| (1u64 << 63) | u64::from(kind.as_u32()))
        .collect::<Vec<_>>();
    let mut seen = DELIVERY_FIRST_SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let seen = seen.get_or_insert_with(Default::default);
    let owner = router as *const sedsnet::router::Router as usize;
    seen.retain(|(router_id, id), _| *router_id != owner || queued.contains(id) || rows.iter().any(|row| row.packet_id == *id));
    let mut fresh = queued.into_iter()
        .filter(|key| now.duration_since(*seen.entry((owner, *key)).or_insert(now)) < window)
        .collect::<Vec<_>>();
    fresh.extend(rows.iter()
        .filter(|row| kinds.contains(&row.data_type) && !row.waiting_for_predecessor)
        // A retired automatic ABORT is never sent again: it holds nothing.
        .filter(|row| !crate::abort_retirement::is_retired(router, row.packet_id))
        .filter(|row| {
            let first = *seen.entry((owner, row.packet_id)).or_insert(now);
            now.duration_since(first) < window
                && u64::from(row.retries).saturating_mul(retry_ms) < window.as_millis() as u64
        })
        .map(|row| row.packet_id));
    Ok(fresh)
}
#[cfg(test)]
pub(crate) fn age_delivery_tracking_for_test(by: std::time::Duration) {
    if let Some(seen) = DELIVERY_FIRST_SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
        for first in seen.values_mut() { *first = first.checked_sub(by).unwrap_or(*first); }
    }
}
/// An ABORT still in its normal delivery window, or a controller frame still
/// on the umbilical, holds Clear GSE abort. An ABORT outstanding longer (a
/// dead or saturated radio link) and the radio writer's own backlog do not:
/// clearing sends no output command and board aborts stay latched on the
/// boards, and a late ABORT only re-aborts its board (the operator is told),
/// so waiting on a frame the network cannot deliver would leave the station
/// aborted forever.
/// HITL never holds Clear GSE abort on delivery (unchanged).
/// Err only while another thread holds the SDK core: nothing was decided.
fn clear_blocked_by_abort_delivery(
    state: &AppState,
    router: &sedsnet::router::Router,
) -> Result<bool, sedsnet::router::TryPendingTypeError> {
    if crate::runtime_mode::is_hitl() {
        return Ok(false);
    }
    Ok(state.has_pending_umbilical_control_tx()
        || !try_commands_still_in_delivery(router, &[crate::telemetry_schema::data_type("ABORT")])?.is_empty())
}
/// Refusal for an operator action whose router check found the SDK core
/// held: nothing was changed, so the same action may be tried again.
pub(crate) const ROUTER_CORE_BUSY: &str = "The network router is busy; try again in a moment.";
/// Output work a station mode change would orphan: the switch resets the
/// sequence engine, so a sequence in progress or cold flow holds it, and each
/// releases the moment that work ends (the engine leaves its active phases).
/// The manual-control latch does not: it stays set after a single manual valve
/// command or an abort's cancel, long after anything moved. Abort and delivery
/// state do not either: a mode change sends no output command.
pub(crate) fn mode_switch_block_reason(state: &AppState) -> Option<&'static str> {
    let rt = state.gse.lock().unwrap();
    if rt.engine.active() {
        return Some("a GSE sequence is running");
    }
    if cold_flow::runtime_active(&rt) {
        return Some("cold flow is running");
    }
    None
}
/// Operator recovery only: never dispatch a valve, ignition or launch command.
fn clear_ground_abort(state: &Arc<AppState>) -> Result<(), (StatusCode, String)> {
    require_primary_operational_station()?;
    let hitl = crate::runtime_mode::is_hitl();
    // Declared before the runtime guard so it is released after it.
    let mut _clear_reservation: Option<automatic_abort::AutomaticAbortClaim> = None;
    let mut rt = state.gse.lock().unwrap();
    let aborted = *state.state.lock().unwrap() == FlightState::Aborted;
    let aborted = aborted
        || (!hitl
            && rt.operational.saved.automatic_abort_claimed
            && matches!(
                state.local_flight_state_snapshot(),
                FlightState::Startup | FlightState::Idle | FlightState::Armed
            ));
    if !aborted {
        return Err((StatusCode::CONFLICT, "GroundStation is not aborted".into()));
    }
    if !hitl {
        if rt.automatic_abort_in_flight {
            return Err((
                StatusCode::CONFLICT,
                "Automatic abort dispatch is still in progress".into(),
            ));
        }
        _clear_reservation = Some(automatic_abort::reserve_abort_clear(state, &mut rt));
    }
    // Preserve the existing validation priority even before a router exists.
    if rt.engine.active() {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    if !hitl && rt.operational.running {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    // Reservation excludes new automatic producers and Fire while ownership is
    // inspected. Do not hold GSE across router/transport locks or callbacks.
    drop(rt);
    let router = state.topology_router.get().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "Network is not initialized".into(),
    ))?;
    let busy = || (StatusCode::SERVICE_UNAVAILABLE, ROUTER_CORE_BUSY.to_string());
    if clear_blocked_by_abort_delivery(state, router).map_err(|_| busy())? {
        return Err((
            StatusCode::CONFLICT,
            "Abort delivery is still pending; retry in a few seconds (an Abort that cannot be delivered stops holding this after 5 s)".into(),
        ));
    }
    // Clearing past an Abort the network could not deliver: that board may
    // still receive it later and abort again. Say so, naming where it is owed.
    let undelivered_abort = !hitl
        && router
            .try_has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT"))
            .map_err(|_| busy())?;
    crate::system_clock::required_control_network_utc_ms(router).map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Recovery requires synchronized network time: {error}"),
        )
    })?;
    let mut rt = state.gse.lock().unwrap();
    if rt.engine.active() {
        return Err((
            StatusCode::CONFLICT,
            "Stop the active sequence first".into(),
        ));
    }
    if !hitl {
        if rt.operational.running {
            return Err((
                StatusCode::CONFLICT,
                "Stop the active sequence first".into(),
            ));
        }
        // Invalidate delayed completions and require explicit requalification.
        rt.operational
            .saved
            .qualification
            .reset()
            .map_err(|e| (StatusCode::CONFLICT, e.into()))?;
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.saved.pending_checklists.clear();
        rt.operational.saved.checklist_digest = None;
        rt.operational.queue_checklist_reset();
        rt.operational
            .persist()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        crate::checklists::reset_sequence_completions()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        rt.operational.persist_error = None;
        rt.operational.qd = QdWait::default();
        rt.operational.qd_launch_admission = None;
        rt.operational.qd_dispatch_utc_ms = None;
        // Do not restart automatic boot valve setup during abort recovery.
        // The next explicit self-test establishes and checks its own baseline.
        rt.operational.startup_configured = true;
    }
    // Update the persisted managed value as well as the local/UI state, so
    // discovery and reconnection cannot replay this station's old abort value.
    drop(rt);
    crate::network_variables::set_flight_state(router, FlightState::Idle as u8)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut rt = state.gse.lock().unwrap();
    if !hitl {
        rt.operational
            .clear_automatic_abort_claim()
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    }
    let config = rt.engine.config.clone();
    rt.progress.complete(
        crate::sequence_progress_projection::TerminalProof::Interrupted,
        crate::sequence_progress_service::now_ms(),
    );
    rt.progress_admission = None;
    rt.engine = station_engine();
    rt.manual_control = false;
    rt.fill_trim = None;
    rt.engine.config = config;
    rt.engine.config.dry_self_test_confirmed = false;
    drop(rt);
    state.set_local_flight_state(FlightState::Idle);
    // Cleared: an automatic ABORT still owed to a board must not land later.
    // Manual Aborts are not registered there and keep their delivery.
    let retired = crate::abort_retirement::retire_on_operator_clear(state, router);
    if undelivered_abort
        && (retired == 0
            || crate::abort_retirement::try_has_pending_reliable_tx_type(router, crate::telemetry_schema::data_type("ABORT")).unwrap_or(true))
    {
        state.add_notification(UNDELIVERED_ABORT_AFTER_CLEAR);
    }
    crate::sequences::refresh_action_policy_now(state);
    state.broadcast_action_policy_snapshot();
    progress::flush(state);
    Ok(())
}
pub(crate) const UNDELIVERED_ABORT_AFTER_CLEAR: &str = "GSE abort cleared while an Abort was still undelivered (most likely to the AV bay over the radio). If that board receives it later, it will abort again and Launch will show it: check that board's link and recover it if it does.";

/// Signed RRLS request at the primary: exactly the local endpoint's checks.
pub(crate) async fn clear_abort_for_proxy(
    state: &Arc<AppState>,
    principal: &crate::auth::AuthPrincipal,
    admission_generation: u64,
) -> StatusCode {
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN;
    }
    let Some(_claim) = claim_operator_dispatch(state, admission_generation) else {
        return StatusCode::CONFLICT;
    };
    match clear_ground_abort_waiting_out_core(state).await {
        Ok(()) => StatusCode::OK,
        Err((status, _)) => status,
    }
}
/// The router check found the SDK core held before anything was cleared.
fn clear_refused_busy(error: &(StatusCode, String)) -> bool {
    error.0 == StatusCode::SERVICE_UNAVAILABLE && error.1 == ROUTER_CORE_BUSY
}
/// Clear GSE abort for an operator: a held SDK core decided nothing, so the
/// whole check-and-clear runs again, briefly, with no guard held between.
async fn clear_ground_abort_waiting_out_core(state: &Arc<AppState>) -> Result<(), (StatusCode, String)> {
    crate::core_busy::retry(|| clear_ground_abort(state), clear_refused_busy).await
}

async fn clear_abort(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let admission_generation = capture_operator_origin(&state).unwrap_or(0);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if crate::rrls::is_rrls() {
        // RRLS has no authority of its own: ask the primary over the signed proxy.
        return crate::command_proxy::submit(
            &state,
            &principal,
            crate::web::bearer_token_from_headers(&headers),
            crate::command_proxy::Operation::Command("ClearGseAbort".into()),
        )
        .await;
    }
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(_claim) = claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            crate::board_recovery::coordinator::admission_closed_message(&state, "Clear GSE abort"),
        )
            .into_response();
    };
    // A held SDK core decided nothing: check and clear again, briefly.
    match clear_ground_abort_waiting_out_core(&state).await {
        Ok(()) => Json(serde_json::json!({"cleared": true})).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn reset_qualification(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let admission_generation = capture_operator_origin(&state).unwrap_or(0);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(p) => p,
            Err(error) => return error,
        };
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if !principal.allows_command_name("ValveSelfTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(_claim) = claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if crate::runtime_mode::is_hitl() {
        (
            StatusCode::CONFLICT,
            "Operational qualification is not used in HITL",
        )
            .into_response()
    } else {
        if !matches!(
            *state.state.lock().unwrap(),
            FlightState::Startup | FlightState::Idle
        ) {
            return (
                StatusCode::CONFLICT,
                "Return to Idle before resetting qualification",
            )
                .into_response();
        }
        let mut rt = state.gse.lock().unwrap();
        if rt.operational.running || cold_flow::runtime_active(&rt) {
            return (StatusCode::CONFLICT, "Stop the active sequence first").into_response();
        }
        if let Err(error) = rt.operational.saved.qualification.reset() {
            return (StatusCode::CONFLICT, error).into_response();
        }
        rt.operational.saved.pending_checklists.clear();
        rt.operational.saved.checklist_digest = None;
        rt.operational.queue_checklist_reset();
        rt.operational.saved.baseline = None;
        rt.operational.saved.fill_target_kg = None;
        rt.operational.persist_error = None;
        rt.operational.qd = QdWait::default();
        rt.operational.qd_launch_admission = None;
        let config = rt.engine.config.clone();
        rt.progress.complete(
            crate::sequence_progress_projection::TerminalProof::Interrupted,
            crate::sequence_progress_service::now_ms(),
        );
        rt.progress_admission = None;
        rt.engine = station_engine();
        rt.engine.config = config;
        rt.engine.config.dry_self_test_confirmed = false;
        rt.operational.startup_configured = false;
        rt.manual_control = false;
        rt.fill_trim = None;
        if let Err(error) = rt.operational.persist() {
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
        if let Err(error) = crate::checklists::reset_sequence_completions() {
            rt.operational.persist_error = Some(error.clone());
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
        drop(rt);
        crate::sequences::refresh_action_policy_now(&state);
        progress::flush(&state);
        Json(serde_json::json!({"reset":true})).into_response()
    }
}

#[cfg(test)]
pub(crate) fn set_operational_prerequisites_for_test(state: &AppState) {
    let mut rt = state.gse.lock().unwrap();
    rt.operational.topology_stable = true;
    rt.operational.startup_configured = true;
    rt.operational
        .saved
        .qualification
        .complete(Milestone::SelfTest, 0)
        .unwrap();
}

/// All GroundStation automatic abort producers share this stage boundary.
/// Manual Abort remains a separate operator command.
pub fn automatic_abort_allowed(state: &AppState) -> bool {
    // A retained manual Stop supersedes new automatic intent. Do not use the
    // shared indicator here: the already-claimed automatic path sets it before
    // its own final SDK admission. Unsafe output admission has its own fence.
    if state.manual_abort_intent.pending_epoch().is_some() || state.deferred_abort.manual_pending()
    {
        return false;
    }
    if crate::runtime_mode::is_hitl() {
        let _runtime = state.gse.lock().unwrap();
        !state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
            && matches!(
                state.local_flight_state_snapshot(),
                FlightState::Startup
                    | FlightState::Idle
                    | FlightState::PreFill
                    | FlightState::FillTest
                    | FlightState::NitrogenFill
                    | FlightState::NitrousFill
                    | FlightState::Armed
            )
    } else {
        let rt = state.gse.lock().unwrap();
        !state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
            && automatic_abort::stage_allows_automatic_abort(
                &rt,
                state.local_flight_state_snapshot(),
            )
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ColdFlowOverrideRequest {
    reason: String,
}
async fn override_cold_flow(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ColdFlowOverrideRequest>,
) -> Response {
    let admission_generation = capture_operator_origin(&state).unwrap_or(0);
    let principal =
        match crate::web::authorize_headers(&state, &headers, Permission::SendCommands).await {
            Ok(principal) => principal,
            Err(error) => return error,
        };
    if let Err(error) = require_primary_operational_station() {
        return error.into_response();
    }
    if principal.anonymous || !principal.allows_command_name("NitrogenTest") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(_claim) = claim_operator_dispatch(&state, admission_generation) else {
        return (
            StatusCode::CONFLICT,
            "Recovery admission is closed or request is stale",
        )
            .into_response();
    };
    if crate::runtime_mode::is_hitl() {
        match apply_hitl_nitrogen_override(
            &state,
            principal.username.as_deref().unwrap_or(""),
            &request.reason,
        ) {
            Ok(()) => {
                crate::sequences::refresh_action_policy_now(&state);
                Json(serde_json::json!({"overridden":true})).into_response()
            }
            Err(error) => (StatusCode::CONFLICT, error).into_response(),
        }
    } else {
        match apply_cold_flow_override(
            &state,
            principal.username.as_deref().unwrap_or(""),
            &request.reason,
        ) {
            Ok(()) => {
                crate::sequences::refresh_action_policy_now(&state);
                Json(serde_json::json!({"overridden":true})).into_response()
            }
            Err(error) => (StatusCode::CONFLICT, error).into_response(),
        }
    }
}
fn apply_cold_flow_override(
    state: &Arc<AppState>,
    operator: &str,
    reason: &str,
) -> Result<(), String> {
    if !matches!(
        *state.state.lock().unwrap(),
        FlightState::Startup | FlightState::Idle
    ) {
        return Err("Return to Idle before overriding cold flow".into());
    }
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    if rt.operational.running
        || rt.engine.active()
        || !rt.operational.topology_stable
        || !rt.operational.startup_configured
        || rt.operational.persist_error.is_some()
        || !interlock(state, &state.action_policy_snapshot())
    {
        return Err(
            "Override requires stable connections, interlock and no active sequence".into(),
        );
    }
    let baseline = rt
        .operational
        .saved
        .baseline
        .clone()
        .ok_or("Run the valve self-test to record an empty-tank baseline first")?;
    if !rt
        .pressure
        .is_some_and(|p| p.at_ms <= now && now - p.at_ms <= 2000 && baseline.is_zero(p.value))
        || ![3, 4].into_iter().all(|i| {
            rt.valves[i].is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2000)
        })
    {
        return Err(
            "Override requires fresh zero-band pressure and both supply valves confirmed closed"
                .into(),
        );
    }
    rt.operational
        .override_cold_flow(gse_sequence::qualification::ColdFlowOverride {
            operator: operator.into(),
            reason: reason.into(),
            at_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
        })?;
    rt.engine.status.nitrogen_overridden = true;
    rt.engine.status.nitrogen_passed = false;
    rt.engine.status.baseline = Some(baseline);
    rt.engine.status.phase = Phase::Passed;
    rt.engine.status.self_test_locked = true;
    rt.engine.claimed = true;
    rt.engine.status.message =
        "Cold-flow requirement overridden by operator; fresh fill interlocks still apply".into();
    Ok(())
}
/// HITL: an operator who knows the tank is good skips the nitrogen test and
/// goes straight to fill. The physical checks still apply: no running
/// sequence, interlock, fresh pressure and both supplies confirmed closed.
/// Without a self-test baseline the current pressure becomes the empty-tank
/// baseline, within the configured zero offset the self-test enforces.
fn apply_hitl_nitrogen_override(state: &AppState, operator: &str, reason: &str) -> Result<(), String> {
    let interlocked = interlock(state, &state.action_policy_snapshot());
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    hitl_nitrogen_override_blocker(&rt, interlocked, now).map_or(Ok(()), Err)?;
    let pressure = rt.pressure.expect("checked fresh above").value;
    let baseline = match rt.engine.status.baseline.clone() {
        Some(baseline) => baseline,
        None => {
            let bound = rt.engine.config.maximum_zero_offset_psi.expect("checked above");
            gse_sequence::Baseline {
                average_psi: pressure,
                min_psi: pressure,
                max_psi: pressure,
                noise_psi: bound,
                samples: 1,
            }
        }
    };
    rt.engine.status.nitrogen_overridden = true;
    rt.engine.status.nitrogen_passed = false;
    rt.engine.status.baseline = Some(baseline);
    rt.engine.status.phase = Phase::Passed;
    rt.engine.status.self_test_locked = true;
    rt.engine.claimed = true;
    rt.engine.status.message = format!(
        "Nitrogen test overridden by {operator}: {reason}. Fresh fill interlocks still apply"
    );
    log::warn!("HITL nitrogen test overridden by {operator}: {reason}");
    Ok(())
}
/// Why the HITL nitrogen-test override is unavailable now, if it is.
fn hitl_nitrogen_override_blocker(rt: &Runtime, interlocked: bool, now: u64) -> Option<String> {
    if rt.engine.active() {
        return Some("Stop the running GSE sequence first".into());
    }
    if !interlocked {
        return Some("Enable the key/software interlock first".into());
    }
    let Some(pressure) = rt.pressure.filter(|p| p.at_ms <= now && now - p.at_ms <= 2000) else {
        return Some("A fresh tank pressure reading is required".into());
    };
    if ![3, 4].into_iter().all(|i| rt.valves[i].is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2000)) {
        return Some("Both supply valves must report closed".into());
    }
    match (&rt.engine.status.baseline, rt.engine.config.maximum_zero_offset_psi) {
        (Some(baseline), _) if !baseline.is_zero(pressure.value) => Some(format!(
            "Tank reads {:.1} psi, outside its empty-tank baseline; vent before filling",
            pressure.value
        )),
        (Some(_), _) => None,
        (None, None) => Some("Set the maximum zero offset in GSE settings first".into()),
        (None, Some(bound)) if pressure.value.abs() > bound => Some(format!(
            "Tank reads {:.1} psi, above the {bound:.1} psi empty-tank limit; vent before overriding",
            pressure.value
        )),
        (None, Some(_)) => None,
    }
}
/// Accepted controller owners must retire before an irreversible launch is saved.
/// This inspects SDK ownership, not modem airtime or physical output feedback.
#[cfg(test)]
fn controller_delivery_pending(state: &AppState) -> bool {
    if manual_stop_pending(state) {
        return true;
    }
    state.topology_router.get().is_some_and(|router| {
        [
            "ABORT",
            "VALVE_COMMAND",
            "ACTUATOR_COMMAND",
            "FLIGHT_COMMAND",
        ]
        .iter()
        .any(|name| router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)))
    })
}

/// Inspect actual accepted controller ownership without waiting for SDK Core.
/// Providers are resolved before DISPATCH; Busy never means clear ownership.
fn launch_controller_delivery_state(
    state: &AppState,
    kinds: &[sedsnet::config::DataType; 4],
) -> CurrentState {
    // HITL: an undeliverable command to an absent board, or a Stop that has not
    // been delivered, must not hold a fresh press. A Stop pressed after this
    // press still cancels it through the ticket's epochs, and a Stop that lands
    // makes the board report Abort, which blocks the next press.
    let hitl = crate::runtime_mode::is_hitl();
    if !hitl && manual_stop_ram_pending(state) {
        return CurrentState::Invalid;
    }
    let Some(router) = state.topology_router.get() else {
        return CurrentState::Current;
    };
    for kind in kinds {
        match router.try_has_pending_reliable_tx_type(*kind) {
            Ok(false) => {}
            Ok(true) if hitl => {}
            Ok(true) => return CurrentState::Invalid,
            Err(sedsnet::router::TryPendingTypeError::CoreBusy) => {
                return CurrentState::PendingBusy;
            }
        }
    }
    CurrentState::Current
}
pub(crate) const LAUNCH_LOCKED_MESSAGE: &str = "Launch is locked after the last attempt. It unlocks when every board on the network reads back healthy in Startup or Idle, or use Reset all boards.";
pub(crate) const LAUNCH_RECOVERY_UNFINISHED_MESSAGE: &str = "Launch is unavailable until the unfinished board recovery completes. Recover the listed boards or use Reset all boards.";
/// Why the launch journal refuses Launch: a real earlier attempt, or an
/// unfinished recovery that still inhibits automation. None when clear.
// Safety catalog (About safety): launch-lock-reset. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn launch_journal_block(history: &crate::launch_history::Store) -> Option<&'static str> {
    if history.launch_locked() {
        Some(LAUNCH_LOCKED_MESSAGE)
    } else if history.automatic_actions_inhibited() {
        Some(LAUNCH_RECOVERY_UNFINISHED_MESSAGE)
    } else {
        None
    }
}
pub(crate) const TURBOCHARGE_UNFINISHED_MESSAGE: &str = "Turbocharge must finish or be cancelled before firing.";
/// Turbocharge is a pre-fire operation. Busy reads as unfinished so contention
/// cannot admit a fire; presentation callers use `turbocharge_active_nowait`.
fn turbocharge_active(rt: &Runtime) -> bool {
    rt.turbocharge.is_some() || rt.engine.status.phase == Phase::Turbocharge
}
/// Waits for the GSE owner: for refusing an operator press, not presentation.
pub(crate) fn turbocharge_active_now(state: &AppState) -> bool {
    turbocharge_active(&state.gse.lock().unwrap())
}
pub(crate) fn turbocharge_active_nowait(state: &AppState) -> bool {
    state.gse.try_lock().map_or(true, |rt| turbocharge_active(&rt))
}
/// Whether turbocharge is active, waiting out a GSE lock that is only busy
/// for the shared core-busy retry window. `Err` means the lock stayed busy,
/// which says nothing about turbocharge.
pub(crate) fn turbocharge_active_retrying(state: &AppState) -> Result<bool, GseBusy> {
    crate::core_busy::retry_blocking(
        || match state.gse.try_lock() {
            Ok(rt) => Ok(turbocharge_active(&rt)),
            Err(std::sync::TryLockError::Poisoned(rt)) => Ok(turbocharge_active(&rt.into_inner())),
            Err(std::sync::TryLockError::WouldBlock) => Err(GseBusy),
        },
        |_| true,
    )
}
/// The GSE runtime stayed locked by other work for the whole retry window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GseBusy;
/// Return the real effect gate ONLY after a nonwaiting clear observation.
/// Every Busy drops DISPATCH/Stop guards before bounded same-ticket retry.
/// No durable Launch inhibition is written on this preparation-only retry path.
fn launch_preparation_gate(
    state: &AppState,
    ticket: LaunchAdmission,
    kinds: &[sedsnet::config::DataType; 4],
) -> Result<std::sync::MutexGuard<'static, ()>, String> {
    let started = Instant::now();
    let window = std::time::Duration::from_millis(100);
    let mut retried = false;
    loop {
        if retried && started.elapsed() >= window {
            return Err(
                "Launch preparation timed out waiting for SDK Core; no launch intent was saved"
                    .into(),
            );
        }
        let dispatch = dispatch_gate();
        if retried && started.elapsed() >= window {
            return Err(
                "Launch preparation timed out waiting for SDK Core; no launch intent was saved"
                    .into(),
            );
        }
        with_stop_admission(state, ticket, || ()).map_err(str::to_string)?;
        match launch_controller_delivery_state(state, kinds) {
            CurrentState::Current => return Ok(dispatch),
            CurrentState::Invalid => return Err(
                "Controller command delivery or Manual Stop is pending; launch was not dispatched"
                    .into(),
            ),
            CurrentState::PendingBusy => {
                drop(dispatch);
                retried = true;
            }
        }
        #[cfg(test)]
        nonwaiting_admission_tests::core_busy_refused(state);
        if started.elapsed() >= window {
            return Err(
                "Launch preparation timed out waiting for SDK Core; no launch intent was saved"
                    .into(),
            );
        }
        std::thread::sleep(
            std::time::Duration::from_millis(5).min(window.saturating_sub(started.elapsed())),
        );
    }
}

pub(crate) fn operator_phase_mode(state:&AppState)->Option<(bool,u64)> {
    state.gse.try_lock().ok().map(|rt|(rt.ground_station_control,rt.phase_control_revision))
}
pub(crate) fn operator_phase_public_context(state:&AppState)->Option<(bool,u64,bool)> {
    state.gse.try_lock().ok().map(|runtime|(runtime.ground_station_control,runtime.phase_control_revision,runtime.cold_flow.is_none()))
}
pub(crate) fn operator_phase_runtime_available(state:&AppState)->bool {
    operator_phase_runtime_context(state)==Some(true)
}
/// None while another owner holds the runtime.
pub(crate) fn operator_phase_runtime_context(state:&AppState)->Option<bool> {
    state.gse.try_lock().ok().map(|runtime|runtime.cold_flow.is_none())
}
pub(crate) fn operator_phase_ticket(state:&AppState,token:crate::operator_queue::Token)->LaunchAdmission {
    LaunchAdmission {board_abort_revision:state.board_abort.abort_revision(),generation:token.generation,
        manual_epoch:token.manual_epoch,observed_manual_epoch:token.observed_manual_epoch}
}
/// Why an operator phase step was not admitted. `Busy` admitted nothing (a
/// held SDK core or a momentarily held phase owner), so the same step may be
/// offered again; `Refused` is final for this request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhaseRefusal {
    Busy(String),
    Refused(String),
}
impl std::fmt::Display for PhaseRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { Self::Busy(text) | Self::Refused(text) => f.write_str(text) }
    }
}
impl From<PhaseRefusal> for String {
    fn from(value: PhaseRefusal) -> Self { value.to_string() }
}
pub(crate) fn commit_operator_phase(
    state: &AppState,
    intent: &crate::operator_phase::Intent,
    prepared: &mut sedsnet::router::PreparedManagedBatch<'_>,
) -> Result<(), PhaseRefusal> {
    let refused = |text: &str| PhaseRefusal::Refused(text.into());
    let dispatch = dispatch_gate();
    if !crate::operator_phase::button_allowed(state)
        || !crate::operator_phase::controls_allowed(state)
    { return Err(refused("Phase authority changed; request a new phase step.")); }
    match intent.admission(state) {
        crate::operator_phase::Admission::Valid => {}
        crate::operator_phase::Admission::Busy => return Err(PhaseRefusal::Busy("Phase authority check busy".into())),
        crate::operator_phase::Admission::Invalid => return Err(refused("Phase authority changed; request a new phase step.")),
    }
    let (expected, target, revision, local) = intent.context();
    let target = target.ok_or_else(|| refused("Use the explicit Fire/Abort/reset action at this boundary."))?;
    let runtime = state.gse.try_lock().map_err(|_| PhaseRefusal::Busy("Phase controller busy".into()))?;
    if !local || runtime.phase_control_revision != revision || !runtime.ground_station_control {
        return Err(refused("Phase authority changed"));
    }
    let mut current = state.state.try_lock().map_err(|_| PhaseRefusal::Busy("Phase state busy".into()))?;
    if *current != expected { return Err(refused("Global phase changed; request a new phase step")); }
    let router = state.topology_router.get().ok_or_else(|| refused("Phase router unavailable"))?;
    // Same Stop -> board-Abort -> SDK Core.try/Schema.try order as automatic
    // phase publication. Preparation, allocation and retirement stay outside.
    let admitted = with_stop_admission(state, intent.ticket(), || {
        state.board_abort.try_admit_automatic_prelaunch(intent.ticket().board_abort_revision, || {
            router.try_commit_network_variable_batch_unless_pending_type(
                prepared, crate::telemetry_schema::types::ABORT,
            )?;
            *current = target;
            Ok::<(), sedsnet::router::TryManagedBatchError>(())
        })
    });
    drop(current); drop(runtime); drop(dispatch);
    match admitted.map_err(refused)?.map_err(|_| refused("Stop or board context changed"))? {
        Ok(()) => Ok(()),
        Err(error @ (sedsnet::router::TryManagedBatchError::CoreBusy
            | sedsnet::router::TryManagedBatchError::SchemaBusy)) =>
            Err(PhaseRefusal::Busy(format!("Phase update was not admitted: {error:?}"))),
        Err(error) => Err(refused(&format!("Phase update was not admitted: {error:?}"))),
    }
}

/// Ownership snapshot taken before launch journal work. No intent lock is
/// held across durable I/O; a changed Stop epoch invalidates final admission.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LaunchAdmission {
    board_abort_revision: u64,
    generation: u64,
    manual_epoch: u64,
    observed_manual_epoch: u64,
}
impl LaunchAdmission {
    fn trackable(&self) -> bool {
        self.manual_epoch != u64::MAX && self.observed_manual_epoch != u64::MAX
    }
    fn capture_for_launch(state: &AppState, revision: Option<u64>) -> Result<Self, String> {
        Self::capture_for_launch_from(state, revision, None)
    }
    /// A held Launch (zero before Launch) keeps the Stop/reset ownership it
    /// had when pressed: any Stop or reset since then refuses admission.
    fn capture_for_launch_from(
        state: &AppState,
        revision: Option<u64>,
        pressed: Option<Self>,
    ) -> Result<Self, String> {
        // Capture Stop first: a concurrent Stop cannot become a fresh epoch
        // between validating an older launch revision and taking its ticket.
        let mut ticket = pressed.unwrap_or_else(|| Self::capture(state));
        if !ticket.trackable() {return Err("Stop epoch exhausted; unsafe launch admission refused".into());}
        let revision = crate::board_abort::validate_launch_revision(state, revision)?;
        ticket.board_abort_revision = revision;
        Ok(ticket)
    }
    fn remains_current(self, state: &AppState) -> bool {
        self.trackable()
            && self.generation == automatic_generation(state)
            && self.board_abort_revision == state.board_abort.abort_revision()
            && self.manual_epoch == state.manual_abort_intent.epoch()
            && self.observed_manual_epoch == state.deferred_abort.manual_epoch()
            && !manual_stop_pending(state)
    }
    pub(crate) fn automatic_board_context_current(self, state: &AppState) -> bool {
        state.board_abort.automatic_revision_current(self.board_abort_revision)
    }
    pub(crate) fn original_ownership(self) -> (u64, u64, u64) {
        (
            self.generation,
            self.manual_epoch,
            self.observed_manual_epoch,
        )
    }
    pub(crate) fn capture(state: &AppState) -> Self {
        Self {
            board_abort_revision: state.board_abort.abort_revision(),
            generation: automatic_generation(state),
            manual_epoch: state.manual_abort_intent.epoch(),
            observed_manual_epoch: state.deferred_abort.manual_epoch(),
        }
    }
}

/// Retention is not physical confirmation. Keep unsafe new controls fenced
/// until the pending Stop owner and its local observation have been handled.
/// A busy SDK owner is temporary uncertainty, never an observed Stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CurrentState {
    Current,
    PendingBusy,
    Invalid,
}
fn manual_stop_ram_pending(state: &AppState) -> bool {
    state.manual_abort_intent.epoch() == u64::MAX
        || state.deferred_abort.manual_epoch() == u64::MAX
        || state.manual_abort_intent.pending_epoch().is_some()
        || state.deferred_abort.manual_pending()
        || state.abort_indicator_latched()
}
/// Nonwaiting inspection for pure RAM publication. Packet admission must use
/// the SDK's atomic pending-type queue operation, not this momentary sample.
pub(crate) fn manual_stop_state(state: &AppState) -> CurrentState {
    if manual_stop_ram_pending(state) {
        return CurrentState::Invalid;
    }
    let Some(router) = state.topology_router.get() else {
        return CurrentState::Current;
    };
    // Generated identity only: no schema lookup/init under application guards.
    match crate::abort_retirement::try_has_pending_reliable_tx_type(router, crate::telemetry_schema::types::ABORT) {
        Ok(false) => CurrentState::Current,
        Ok(true) => CurrentState::Invalid,
        Err(sedsnet::router::TryPendingTypeError::CoreBusy) => CurrentState::PendingBusy,
    }
}
pub(crate) fn manual_stop_pending(state: &AppState) -> bool {
    manual_stop_ram_pending(state)
        || state.topology_router.get().is_some_and(|router| {
            crate::abort_retirement::has_pending_reliable_tx_type(router, crate::telemetry_schema::data_type("ABORT"))
        })
}
// Safety catalog (About safety): manual-stop. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn abort_safe_control(kind: sedsnet::config::DataType, payload: &[u8]) -> bool {
    if payload.len() != 1 {
        return false;
    }
    if kind == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
        // Existing explicit manual FC stop only. The high bit is not an FC
        // automatic provenance tag and must never alias this permission.
        return payload[0] == crate::rocket_commands::FlightComputerCommands::EvaluationAbort as u8;
    }
    let command = payload[0] & 0x7f;
    if kind == crate::telemetry_schema::data_type("VALVE_COMMAND") {
        command == ValveBoardCommands::PilotClose as u8
            || command == ValveBoardCommands::NormallyOpenOpen as u8
            || command == ValveBoardCommands::DumpOpen as u8
    } else if kind == crate::telemetry_schema::data_type("ACTUATOR_COMMAND") {
        command == ActuatorBoardCommands::IgniterOff as u8
            || command == ActuatorBoardCommands::NitrogenClose as u8
            || command == ActuatorBoardCommands::NitrousClose as u8
            // Tank overpressure: the AB enters its Abort safe state (all
            // outputs off). Automatic origin only; never a manual control.
            || payload[0] == overpressure::AB_ABORT_PAYLOAD
    } else {
        false
    }
}
/// Recording publication shares original Stop ordering. The same fixed
/// one-shot plan survives Busy; only its RAM store runs inside the final gate.
/// No pool/GSE/SDK/filesystem operation may run inside that plan.
pub(crate) fn with_fill_recording_publication(
    state: &AppState,
    origin: &crate::fill_recording::Origin,
    publication: &mut crate::fill_recording::Publication,
) -> Result<(), crate::fill_recording::Error> {
    use crate::fill_recording::Error;
    let _dispatch = dispatch_gate();
    origin.remaining()?;
    if state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0 {
        return Err(Error::Stale);
    }
    match manual_stop_state(state) {
        CurrentState::Current => {},
        CurrentState::PendingBusy => return Err(Error::PendingBusy),
        CurrentState::Invalid => return Err(Error::Stale),
    }
    with_stop_admission(state, origin.ticket(), || publication.apply(state))
        .map_err(|_| Error::Stale)?
}
pub(crate) fn fill_origin_current(state: &AppState, ticket: LaunchAdmission) -> CurrentState {
    let _dispatch = dispatch_gate();
    if state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0
        || with_stop_admission(state, ticket, || ()).is_err() {
        return CurrentState::Invalid;
    }
    manual_stop_state(state)
}
pub(crate) fn recording_publication_gate() -> std::sync::MutexGuard<'static, ()> {
    dispatch_gate()
}
pub(crate) fn revoke_fill_recording(state: &AppState) {
    let _dispatch = dispatch_gate();
    state.fill_recording.revoke_under_dispatch();
}

/// Ordinary operators retain OperatorDispatchClaim through queue/flush; a
/// verified Reset must quiesce those leases before advancing generation.
/// Automatic apply already owns DISPATCH_GATE. Re-entering it here would
/// deadlock; this unit exposes no reset API without that quiescence contract.
pub(crate) fn plumbing_admission_ticket(state: &AppState) -> LaunchAdmission {
    LaunchAdmission::capture(state)
}
pub(crate) fn with_plumbing_admission<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    admit: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    with_plumbing_admission_inner(state, ticket, admit, false)
}
fn with_plumbing_admission_inner<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    admit: impl FnOnce() -> T,
    prepared: bool,
) -> Result<T, &'static str> {
    let _dispatch = dispatch_gate();
    if manual_stop_pending(state) {
        return Err("Manual Stop is pending; plumbing admission refused");
    }
    if prepared {
        #[cfg(test)]
        nonwaiting_admission_tests::after_prepared(state);
    }
    with_stop_admission(state, ticket, admit)
}
/// RAM-only original context. The motion callback MUST use atomic SDK
/// pending-ABORT admission; no blocking SDK inspector may run under this gate.
pub(crate) fn with_motion_context<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    admit: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    let _dispatch = dispatch_gate();
    if manual_stop_ram_pending(state) {
        return Err("Manual Stop is pending; plumbing admission refused");
    }
    with_stop_admission(state, ticket, admit)
}
pub(crate) fn with_prepared_motion_admission<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    first_attempt: bool,
    admit: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    #[cfg(not(test))]
    let _ = first_attempt;
    let _dispatch = dispatch_gate();
    if manual_stop_ram_pending(state) {
        return Err("Manual Stop is pending; plumbing admission refused");
    }
    #[cfg(test)]
    if first_attempt { nonwaiting_admission_tests::after_prepared(state); }
    with_stop_admission(state, ticket, admit)
}
#[cfg(test)]
pub(crate) fn motion_core_busy_refused_for_test(state: &AppState) {
    nonwaiting_admission_tests::core_busy_refused(state);
}
pub(crate) fn queue_plumbing_under_dispatch_with_stop_fence(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    origin: Option<crate::actuator_plumbing::MotionOrigin>,
    logged: bool,
) -> Result<(), String> {
    if manual_stop_pending(state) {
        return Err("Manual Stop is pending; plumbing motion was not queued".into());
    }
    let ticket = LaunchAdmission::capture(state);
    crate::actuator_plumbing::schedule_motion_under_dispatch(state, router, origin, logged, ticket)
}
pub(crate) fn queue_plumbing_with_stop_fence(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    origin: Option<crate::actuator_plumbing::MotionOrigin>,
) -> Result<(), String> {
    let _dispatch = dispatch_gate();
    queue_plumbing_under_dispatch_with_stop_fence(state, router, origin, false)
}

/// Caller already owns DISPATCH; reserve a RAM-only owned worker with the
/// same board→Stop order as final Launch admission. No IO/SDK in reserve.
pub(crate) fn reserve_plumbing_worker(
    state: &AppState,
    ticket: LaunchAdmission,
    reserve: impl FnOnce() -> bool,
) -> bool {
    let _dispatch = dispatch_gate();
    reserve_plumbing_worker_under_dispatch(state, ticket, reserve)
}
pub(crate) fn reserve_plumbing_worker_under_dispatch(
    state: &AppState,
    ticket: LaunchAdmission,
    reserve: impl FnOnce() -> bool,
) -> bool {
    state
        .board_abort
        .change_launch_context_if(|| with_stop_admission(state, ticket, reserve).unwrap_or(false))
}
pub(crate) fn queue_controller_with_stop_fence(
    state: &AppState,
    router: &sedsnet::router::Router,
    kind: sedsnet::config::DataType,
    payload: &[u8],
) -> sedsnet::TelemetryResult<bool> {
    let revision = crate::board_abort::controller_can_launch(kind, payload)
        .then(|| state.board_abort.abort_revision());
    queue_controller_with_launch_revision(state, router, kind, payload, revision)
}
pub(crate) fn queue_controller_with_launch_revision(
    state: &AppState,
    router: &sedsnet::router::Router,
    kind: sedsnet::config::DataType,
    payload: &[u8],
    launch_revision: Option<u64>,
) -> sedsnet::TelemetryResult<bool> {
    let ticket = if crate::board_abort::controller_can_launch(kind, payload) {
        let Ok(ticket) = LaunchAdmission::capture_for_launch(state, launch_revision) else {
            return Ok(false);
        };
        ticket
    } else {
        LaunchAdmission::capture(state)
    };
    let pair_command = crate::ignition_pair::packet_requires_pair(kind, payload);
    let pair_required = pair_command && crate::ignition_pair::proof_enforced(state);
    if pair_command && !crate::ignition_pair::same_router(state,router) { return Ok(false); }
    let packet = crate::telemetry_task::controller_packet(router, kind, payload)?;
    // Only the explicit hardware abort-safe directions bypass the fence.
    if abort_safe_control(kind, payload) {
        router.tx_queue(packet)?;
        return Ok(true);
    }
    if manual_stop_pending(state) {
        return Ok(false);
    }
    #[cfg(test)]
    nonwaiting_admission_tests::after_prepared_packet(state,&packet);
    let started=Instant::now();
    loop {
        if state.abort_indicator_latched() {return Ok(false);}
        let route_token = if pair_required {
            if !state.board_abort.ignition_pair_current() {
                state.board_abort.request_queries(crate::board_abort::required_mask(state)
                    | crate::ignition_pair::REQUIRED_MASK);
                return Ok(false);
            }
            Some(crate::ignition_pair::capture_route_token(state).map_err(sedsnet::TelemetryError::Io)?)
        } else { None };
    let attempt_packet=packet.clone();
    let offer = || {
        if let Some(token)=route_token {
            crate::ignition_pair::validate_route_token(state,token).map_err(sedsnet::TelemetryError::Io)?;
        }
        router.try_tx_queue(attempt_packet)
    };
    let result = if crate::board_abort::controller_can_launch(kind, payload) {
        state
            .board_abort
            .admit_launch(
                state,
                crate::board_abort::required_mask(state)
                    | if pair_required { crate::ignition_pair::REQUIRED_MASK } else { 0 },
                ticket.board_abort_revision,
                || with_stop_admission(state, ticket, offer),
            )
            .and_then(|v| v.map_err(|error| error.to_string()))
    } else if pair_command {
        with_stop_admission(state, ticket, || {
            state.board_abort.admit_ignition(ticket.board_abort_revision, pair_required,
                offer)
        }).and_then(|result| result).map_err(str::to_string)
    } else {
        with_stop_admission(state, ticket, offer)
            .map_err(|error| error.to_string())
    };
    match result {
        Ok(Err(sedsnet::TelemetryError::Io("router core busy")))
            if started.elapsed()<std::time::Duration::from_millis(100)=>{
                #[cfg(test)]
                nonwaiting_admission_tests::core_busy_refused(state);
                #[cfg(test)]
                nonwaiting_admission_tests::launch_retry_boundary(state);
                // No SDK owner exists. Sleep outside board/Stop locks and
                // retry this exact original packet/ticket, never a new intent.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        Ok(result)=>return result.map(|()|{
            if kind == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                crate::operator_command_resend::note_queued(router, packet.packet_id());
            }
            true
        }),
        Err(_)=>return Ok(false),
    }
    }
}

/// Both physical SDK admission and the pure Testing model share this short
/// Stop boundary. The callback must not flush, await, log, or lock GSE/journal/DB.
fn with_stop_admission<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    admit: impl FnOnce() -> T,
) -> Result<T, &'static str> {
    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) { return Err("Ground station is shutting down"); }
    if !ticket.trackable() {
        return Err("Stop epoch exhausted; unsafe control/launch admission refused");
    }
    if ticket.generation != automatic_generation(state) {
        return Err("Reset invalidated launch admission; no launch effect was admitted");
    }
    state
        .manual_abort_intent
        .admit_if_unchanged(ticket.manual_epoch, || {
            state
                .deferred_abort
                .admit_if_no_manual_since(ticket.observed_manual_epoch, || {
                    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) { None } else { Some(admit()) }
                }).flatten()
        })
        .flatten()
        .ok_or("Manual Abort invalidated launch admission; no launch effect was admitted")
}

fn with_launch_admission<T>(
    state: &AppState,
    ticket: LaunchAdmission,
    ignition_pair_required: bool,
    admit: impl FnOnce() -> T,
) -> Result<T, String> {
    // Observe routes before taking any application owner. Metadata-only state
    // publication retains its original mode policy; it is not an igniter command.
    let require_fresh_pair = ignition_pair_required && crate::ignition_pair::proof_enforced(state);
    let route_token = if require_fresh_pair {
        if !state.board_abort.ignition_pair_current() {
            state.board_abort.request_queries(crate::board_abort::required_mask(state)
                | crate::ignition_pair::REQUIRED_MASK);
            return Err("Ignition requires fresh non-Abort Actuator and Valve Board readbacks".into());
        }
        Some(crate::ignition_pair::capture_route_token(state).map_err(str::to_string)?)
    } else { None };
    let _dispatch = dispatch_gate();
    if cold_flow::active(state){return Err("Rocket Launch is inhibited while cold flow owns the valves".into());}
    with_stop_admission(state, ticket, || ()).map_err(str::to_string)?;
    let required = crate::board_abort::required_mask(state)
        | if require_fresh_pair { crate::ignition_pair::REQUIRED_MASK } else { 0 };
    state
        .board_abort
        .admit_launch(state, required, ticket.board_abort_revision, || {
            if let Some(token)=route_token {
                crate::ignition_pair::validate_route_token(state,token)?;
            }
            with_stop_admission(state, ticket, admit)
        })
        .and_then(|v| v.map_err(|error| error.to_string()))
}

/// Registry SDK438 try_tx_queue does not wait for the router-core mutex or
/// invoke local/side callbacks. Schema/configuration and allocation may still
/// contend; no universal wait-free bound is promised. Packets are prepared
/// before entering the short intent locks. A second admission can fail
/// after the first succeeds; that is not an atomic batch or an automatic retry.
pub(crate) fn queue_admitted_launch_packets(
    state: &AppState,
    router: &sedsnet::router::Router,
    ticket: LaunchAdmission,
    packets: Vec<sedsnet::packet::Packet>,
) -> Result<usize, String> {
    with_stop_admission(state, ticket, || ()).map_err(str::to_string)?;
    let prepared = if crate::runtime_mode::is_hitl() {
        None
    } else {
        Some(
            crate::network_variables::prepare_launch_state(router).map_err(|e| {
                format!("Launch preparation failed with no SDK launch admitted: {e}")
            })?,
        )
    };
    queue_prepared_launch_packets(state, router, ticket, packets, prepared)
}

pub(crate) fn queue_state_launch(
    state: &AppState,
    router: &sedsnet::router::Router,
    launch_revision: Option<u64>,
) -> Result<usize, String> {
    let ticket = LaunchAdmission::capture_for_launch(state, launch_revision)?;
    let prepared =
        crate::network_variables::prepare_launch_state(router).map_err(|e| e.to_string())?;
    queue_prepared_launch_packets(state, router, ticket, Vec::new(), Some(prepared))
}

pub(crate) fn queue_prepared_launch_packets(
    state: &AppState,
    router: &sedsnet::router::Router,
    ticket: LaunchAdmission,
    mut packets: Vec<sedsnet::packet::Packet>,
    prepared: Option<crate::network_variables::PreparedLaunchState>,
) -> Result<usize, String> {
    if let Some(prepared) = &prepared {
        packets.insert(0, prepared.packet.clone());
    }
    let ignition_pair_required = packets.iter().any(|packet|
        crate::ignition_pair::packet_requires_pair(packet.data_type(), packet.payload()));
    if ignition_pair_required && !crate::ignition_pair::same_router(state,router) {
        return Err("Ignition route observation belongs to a different SDK router".into());
    }
    let total = packets.len();
    #[cfg(test)] nonwaiting_admission_tests::record_launch_packets(state,&packets);
    #[cfg(test)]
    nonwaiting_admission_tests::after_prepared(state);
    // A NEW retry window for this one prepared Launch, not an ingress TTL.
    // Each original remains owned here. Packet::clone may allocate, so build
    // attempt slots before entering any dispatch/board/Stop admission guard.
    // Legacy try_tx_queue consumes its argument even on refusal; it retains
    // the existing schema/allocation/queue policy and is not a realtime API.
    enum QueueAttempt {
        WindowExpired,
        Completed(Result<usize, (usize, sedsnet::TelemetryError)>),
    }
    let retry_window = std::time::Duration::from_millis(100);
    let mut first_sdk_attempt: Option<Instant> = None;
    let mut last_zero_busy = None;
    let mut retry_controller_types = None;
    // Packets already owned by the SDK in an earlier attempt of this same
    // prepared Launch. A zero-owner CoreBusy refusal of a later packet admits
    // nothing, so (flight builds) only the unadmitted suffix of the SAME
    // vector is offered again inside the same window, after the same
    // admission checks; an accepted packet is never offered twice. Test-fire
    // keeps the original no-suffix-retry behavior.
    let mut admitted_prefix = 0usize;
    let suffix_retry = !crate::runtime_mode::is_test_fire();
    let result = loop {
        if first_sdk_attempt.is_some_and(|at| at.elapsed() >= retry_window) {
            break Err((admitted_prefix, last_zero_busy.take().expect("retry follows a real zero-owner CoreBusy")));
        }
        if last_zero_busy.is_some() && admitted_prefix == 0 {
            // A zero-owner refusal must not outlive a newly retained controller
            // command. These conservative snapshots run outside ALL app guards;
            // they are not an atomic multi-type check-and-queue operation.
            with_stop_admission(state, ticket, || ()).map_err(str::to_string)?;
            let mut inspection_busy = false;
            // The generator exports only a subset of types. Resolve these
            // existing configured names once, outside all application guards,
            // and only when a real zero-owner Busy made a retry necessary.
            let controller_types = retry_controller_types.get_or_insert_with(|| {
                ["ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND", "FLIGHT_COMMAND"]
                    .map(crate::telemetry_schema::data_type)
            });
            for kind in controller_types.iter().copied() {
                match crate::abort_retirement::try_has_pending_reliable_tx_type(router, kind) {
                    Ok(false) => {}
                    Ok(true) if crate::runtime_mode::is_hitl() => {}
                    Ok(true) => return Err("Controller command delivery is pending; launch was not dispatched".into()),
                    Err(sedsnet::router::TryPendingTypeError::CoreBusy) => {
                        inspection_busy = true;
                        break;
                    }
                }
            }
            if inspection_busy {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
        }
        let mut attempt_packets = packets[admitted_prefix..].iter().cloned().map(Some).collect::<Vec<_>>();
        let guarded = with_launch_admission(state, ticket, ignition_pair_required, || {
            let mut accepted = 0;
            for slot in &mut attempt_packets {
                // A guard may have waited. No retry call starts after this
                // window; after a non-busy refusal, preserve legacy behavior.
                if accepted == 0
                    && first_sdk_attempt.is_some_and(|at| at.elapsed() >= retry_window)
                {
                    return QueueAttempt::WindowExpired;
                }
                let packet = slot.take().expect("each attempt packet is consumed once");
                first_sdk_attempt.get_or_insert_with(Instant::now);
                #[cfg(test)] nonwaiting_admission_tests::record_launch_sdk_call(state, first_sdk_attempt);
                if let Err(error) = router.try_tx_queue(packet) {
                    #[cfg(test)] nonwaiting_admission_tests::record_launch_sdk_return(state, accepted, Some(&error));
                    return QueueAttempt::Completed(Err((accepted, error)));
                }
                #[cfg(test)] nonwaiting_admission_tests::record_launch_sdk_return(state, accepted, None);
                accepted += 1;
            }
            QueueAttempt::Completed(Ok(accepted))
        });
        // In particular, drop all unattempted clones outside application
        // guards. SDK consumption of an attempted clone is unchanged legacy.
        drop(attempt_packets);
        let attempt = match guarded {
            Ok(QueueAttempt::WindowExpired) => {
                break Err((admitted_prefix, last_zero_busy.take().expect("expired retry retains its actual refusal")));
            }
            Ok(QueueAttempt::Completed(attempt)) => attempt
                .map(|n| admitted_prefix + n)
                .map_err(|(n, error)| (admitted_prefix + n, error)),
            // A refused suffix re-admission (new Abort, Stop, stale proof)
            // still reports the packets the SDK already owns.
            Err(message) if admitted_prefix > 0 => {
                log::warn!("Launch suffix re-admission refused after {admitted_prefix}/{total} packets: {message}");
                break Err((admitted_prefix, sedsnet::TelemetryError::Io(
                    "launch suffix re-admission refused")));
            }
            Err(message) => return Err(message),
        };
        match attempt {
            Err((0, error @ sedsnet::TelemetryError::Io("router core busy"))) => {
                #[cfg(test)] nonwaiting_admission_tests::core_busy_refused(state);
                #[cfg(test)] nonwaiting_admission_tests::launch_retry_boundary(state);
                last_zero_busy = Some(error);
                // All admission guards have returned. Keep the same ticket,
                // vector, IDs, UTC and optional PreparedLaunchState; never
                // reprepare or rerun an accepted prefix as a new intention.
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err((owned, error @ sedsnet::TelemetryError::Io("router core busy"))) if suffix_retry => {
                #[cfg(test)] nonwaiting_admission_tests::core_busy_refused(state);
                admitted_prefix = owned;
                last_zero_busy = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            terminal => break terminal,
        }
    };
    drop(packets);
    let accepted = match &result {
        Ok(n) => *n,
        Err((n, _)) => *n,
    };
    // Even a partial batch has real SDK owners. Persist/describe that admitted
    // Launch truthfully; no implicit retry and no automatic Abort are allowed.
    if let Some(prepared) = prepared.filter(|_| accepted > 0) {
        let _dispatch = dispatch_gate();
        with_stop_admission(state,ticket,||()) .map_err(|e|format!("SDK admitted {accepted}/{total} Launch packets, but {e}; explicit recovery required"))?;
        let claim=claim_operator_dispatch_under_gate(state,ticket.generation).ok_or_else(||format!("SDK admitted {accepted}/{total} Launch packets, recovery closed before publication"))?;
        crate::network_variables::commit_admitted_launch(&prepared).map_err(|e| {
            format!(
                "SDK admitted {accepted}/{total} Launch packets, desired publication failed: {e}"
            )
        })?;
        with_stop_admission(state,ticket,||()).map_err(|e|format!("SDK admitted {accepted}/{total} Launch packets, newer Stop prevents local Launch publication: {e}"))?;
        state.set_local_flight_state_with_clock_claim(FlightState::Launch, &claim);
    }
    result.map_err(|(accepted,error)|format!("Launch SDK admission accepted {accepted}/{total} packets: {error}; launch remains inhibited, explicit manual recovery required"))
}

/// Testing mode only (the old testing build was the only one with it).
pub(crate) fn admit_simulated_retraction_stamped(
    state: &AppState,
    origin: Option<crate::actuator_plumbing::MotionOrigin>,
    effect: impl FnOnce(u64,u64) -> bool,
) -> Result<(), String> {
    if !crate::runtime_mode::is_testing() {
        return Err(crate::runtime_mode::unavailable("Modeled retraction"));
    }
    let ticket = LaunchAdmission::capture(state);
    let router = state
        .topology_router
        .get()
        .ok_or("Testing telemetry router unavailable")?;
    // Sampling/validation is outside the shared admission boundary.
    let source = crate::actuator_plumbing::validated_motion_stamp(state, router, origin)?;
    let observed_utc=crate::system_clock::required_control_network_utc_ms(router).map_err(|_|"Testing canonical UTC unavailable")?;
    let _dispatch = dispatch_gate();
    let mut accepted = false;
    state.board_abort.change_launch_context_if(|| {
        with_stop_admission(state, ticket, || {
            accepted = effect(source,observed_utc);
            accepted && crate::actuator_plumbing::record_modeled_retraction(state, source, ticket)
        })
        .unwrap_or(false)
    });
    if accepted {
        Ok(())
    } else {
        Err("The model did not accept the original retraction intent".into())
    }
}

pub(crate) fn admit_simulated_launch(
    state: &AppState,
    ticket: LaunchAdmission,
    launch: impl FnOnce() -> bool,
) -> Result<(), String> {
    let accepted =
        with_launch_admission(state, ticket, true, launch).map_err(|error| error.to_string())?;
    if accepted {
        Ok(())
    } else {
        Err(
            "Testing model declined launch; inhibition retained, no physical fallback permitted"
                .into(),
        )
    }
}

/// Serialize launch admission against automatic valve-effect preparation in every mode.
/// T-clock reset and delayed flight-state packets cannot re-enable automation.
pub(crate) fn inhibit_automatic_actions_for_launch_revision(
    state: &Arc<AppState>,
    launch_revision: Option<u64>,
) -> Result<LaunchAdmission, String> {
    inhibit_automatic_actions_for_pressed_launch(state, launch_revision, None)
}

/// `pressed` is the ticket captured when a held Launch was pressed.
pub(crate) fn inhibit_automatic_actions_for_pressed_launch(
    state: &Arc<AppState>,
    launch_revision: Option<u64>,
    pressed: Option<LaunchAdmission>,
) -> Result<LaunchAdmission, String> {
    // Freeze the original ticket once, before provider work or retry waits.
    let ticket = LaunchAdmission::capture_for_launch_from(state, launch_revision, pressed)?;
    if !ticket.trackable() {
        return Err("Stop epoch exhausted; launch was not admitted".into());
    }
    let controller_types=["ABORT","VALVE_COMMAND","ACTUATOR_COMMAND","FLIGHT_COMMAND"]
        .map(crate::telemetry_schema::data_type);
    let _dispatch=launch_preparation_gate(state,ticket,&controller_types)?;
    state
        .board_abort
        .admit_launch(
            state,
            crate::board_abort::required_mask(state),
            ticket.board_abort_revision,
            || (),
        )
        .map_err(|error| error.to_string())?;
    let runtime = state.gse.lock().unwrap();
    if let Some(reason) = launch_journal_block(&state.launch_history.lock().unwrap()) {
        return Err(reason.into());
    }
    if runtime.automatic_abort_in_flight {
        return Err("Automatic abort is pending; launch was not dispatched".into());
    }
    if turbocharge_active(&runtime) {
        return Err(TURBOCHARGE_UNFINISHED_MESSAGE.into());
    }
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .map(|()| ticket)
        .map_err(|error| {
            format!("Launch was not dispatched: cannot save automatic-action inhibition: {error}")
        })
}

#[cfg(test)]
pub(crate) fn inhibit_automatic_actions_for_launch(
    state: &Arc<AppState>,
) -> Result<LaunchAdmission, String> {
    inhibit_automatic_actions_for_launch_revision(
        state,
        crate::board_abort::capture_launch_revision_named(state, "Launch"),
    )
}

/// Commit once before dispatch, so service restart never repeats an irreversible Fire intent.
#[cfg(test)]
pub(crate) fn commit_fire_intent_revision(
    state: &Arc<AppState>,
    launch_revision: Option<u64>,
) -> Result<LaunchAdmission, String> {
    commit_fire_intent_for_pressed_launch(state, launch_revision, None)
}

/// `pressed` is the ticket captured when a held Launch was pressed.
pub(crate) fn commit_fire_intent_for_pressed_launch(
    state: &Arc<AppState>,
    launch_revision: Option<u64>,
    pressed: Option<LaunchAdmission>,
) -> Result<LaunchAdmission, String> {
    // Freeze the original ticket once, before provider work or retry waits.
    let ticket = LaunchAdmission::capture_for_launch_from(state, launch_revision, pressed)?;
    if !ticket.trackable() {
        return Err("Stop epoch exhausted; launch was not admitted".into());
    }
    let controller_types=["ABORT","VALVE_COMMAND","ACTUATOR_COMMAND","FLIGHT_COMMAND"]
        .map(crate::telemetry_schema::data_type);
    let _dispatch=launch_preparation_gate(state,ticket,&controller_types)?;
    state
        .board_abort
        .admit_launch(
            state,
            crate::board_abort::required_mask(state),
            ticket.board_abort_revision,
            || (),
        )
        .map_err(|error| error.to_string())?;
    let _clock_claim = claim_operator_dispatch_under_gate(state, ticket.generation)
        .ok_or_else(|| "Recovery admission is closed; Fire was not admitted".to_string())?;
    let mut rt = state.gse.lock().unwrap();
    if let Some(reason) = launch_journal_block(&state.launch_history.lock().unwrap()) {
        return Err(reason.into());
    }
    if rt.operational.saved.automatic_abort_claimed || rt.automatic_abort_in_flight {
        return Err("Automatic abort is pending; clear GSE abort before Fire".into());
    }
    if turbocharge_active(&rt) {
        return Err(TURBOCHARGE_UNFINISHED_MESSAGE.into());
    }
    if !prelaunch(state)
        || !operational_launch_allowed(state, &rt)
        || !interlock(state, &state.action_policy_snapshot())
    {
        return Err("Fire prerequisites changed; launch was not dispatched".into());
    }
    if !crate::runtime_mode::is_test_fire()
        && !matches!(rt.operational.qd.status(rt.now()), QdStatus::Ready)
    {
        return Err("Quick disconnect is not acknowledged and settled".into());
    }
    rt.operational.commit_fire()?;
    state.launch_history.lock().unwrap().inhibit_automatic_actions()
        .map_err(|error| format!("Fire intent saved, but launch inhibition could not be saved; launch was not dispatched: {error}"))?;
    rt.engine.claimed = true;
    // From this point even a checklist-storage failure must not re-arm the operation.
    let checklist_result = rt.operational.flush_checklists();
    drop(rt);
    // Actual Launch publication follows final SDK admission, never preparation.
    checklist_result.map(|()| ticket).map_err(|error| {
        format!("Fire intent saved, but checklist update failed; manual recovery required: {error}")
    })
}

#[cfg(test)]
pub(crate) fn commit_fire_intent(state: &Arc<AppState>) -> Result<LaunchAdmission, String> {
    commit_fire_intent_revision(
        state,
        crate::board_abort::capture_launch_revision_named(state, "Launch"),
    )
}

/// Modeled prerequisite for older unit fixtures. Actual nonce-correlated SDK
/// readback and restart/missing-proof refusal are covered by coordinator tests.
#[cfg(test)]
pub(crate) fn qualified_launch_proofs_for_test(state:&AppState) {
    for board in crate::board_abort::BOARDS {
        let at=Instant::now();
        state.board_abort.observe_status(board,42,8,1,Some(1),false,true,at,0,at,1);
    }
}
#[cfg(test)]
pub(crate) fn armed_fixture_for_test(
    state: &Arc<AppState>,
    router: &Arc<sedsnet::router::Router>,
) -> std::path::PathBuf {
    armed_fixture_network_for_test(state,router,true)
}
#[cfg(test)]
pub(crate) fn armed_fixture_with_real_pair_for_test(state:&Arc<AppState>,router:&Arc<sedsnet::router::Router>)->std::path::PathBuf {
    armed_fixture_network_for_test(state,router,false)
}
#[cfg(test)]
fn armed_fixture_network_for_test(state:&Arc<AppState>,router:&Arc<sedsnet::router::Router>,modeled_network:bool)->std::path::PathBuf {
    use crate::types::Board;
    crate::telemetry_task::refresh_host_network_time(
        router,
        crate::telemetry_task::get_current_timestamp_ms(),
    );
    state.set_local_flight_state(FlightState::Armed);
    if modeled_network {
    let side = router.add_side_packed("test_fixture_other_boards", |_| Ok(()));
    let mut endpoints = vec![crate::telemetry_schema::endpoint("ACTUATOR_BOARD")];
    if !crate::runtime_mode::is_test_fire() {
        endpoints.push(crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"));
    }
    let announce = sedsnet::discovery::build_discovery_announce("GB", 123, &endpoints).unwrap();
    router
        .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side)
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    }
    if state.topology_router.get().is_none() {
        state.topology_router.set(router.clone()).unwrap();
    }
    for board in [
        Board::GatewayBoard,
        Board::ValveBoard,
        Board::ActuatorBoard,
        Board::DaqBoard,
    ] {
        if !modeled_network&&matches!(board,Board::ActuatorBoard|Board::ValveBoard){continue;}
        state.mark_board_seen(
            board.sender_id(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
    }
    if !crate::runtime_mode::is_test_fire() {
        state.mark_board_seen("FC", crate::telemetry_task::get_current_timestamp_ms());
        crate::antenna::observe(&crate::types::TelemetryRow {
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
            data_type: "GPS_DATA".into(),
            sender_id: "RF".into(),
            values: vec![Some(43.0), Some(-78.0), Some(150.0)],
        });
    }
    let path =
        std::env::temp_dir().join(format!("gse-armed-fixture-{}.json", rand::random::<u128>()));
    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    let mut rt = state.gse.lock().unwrap();
    rt.clock = Instant::now() - std::time::Duration::from_secs(10);
    rt.operational.path = Some(path.clone());
    rt.operational.topology_stable = true;
    rt.operational.startup_configured = true;
    rt.operational.saved.baseline = Some(gse_sequence::Baseline {
        average_psi: 0.0,
        min_psi: 0.0,
        max_psi: 0.0,
        noise_psi: 0.1,
        samples: 50,
    });
    for milestone in [
        Milestone::SelfTest,
        Milestone::NitrogenTest,
        Milestone::FillTarget,
    ] {
        rt.operational
            .saved
            .qualification
            .complete(milestone, 0)
            .unwrap();
    }
    rt.operational.saved.fill_target_kg = Some(target);
    rt.operational.qd.dispatched(0);
    rt.operational.qd.confirm(1);
    rt.operational.persist().unwrap();
    rt.engine.status.phase = Phase::Paused;
    rt.observe_pressure(Some(0.0));
    rt.observe_mass(Some(target));
    for key in KEYS {
        rt.observe_valve(key, false);
    }
    drop(rt);
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    // Return a fresh MODELED prerequisite after unrelated setup completes.
    // This is the fixture's one original seed, never a post-intent refresh.
    if modeled_network {qualified_launch_proofs_for_test(state);} else {
        // Explicit MODELED GB/DAQ operational prerequisites only. AB/VB are
        // proved by genuine same-Router QUERY replies in the caller. There is
        // no FC/AV-bay presence, fake ACTUATOR announcement or AB/VB proof seed.
        for board in [Board::GatewayBoard,Board::DaqBoard]{
            let at=Instant::now();state.board_abort.observe_status(board,42,8,1,Some(1),false,true,at,0,at,1);
        }
    }
    path
}

#[cfg(test)]
pub(crate) fn reset_plumbing_qd_for_test(state: &AppState) {
    state.gse.lock().unwrap().operational.qd = QdWait::default();
}

#[cfg(test)]
pub(crate) fn plumbing_qd_probe_for_test(state: &AppState, advance_ms: u64) -> (QdStatus, u64) {
    assert!(
        crate::simulation_clock_ms().is_none(),
        "test explicitly models monotonic elapsed time"
    );
    let mut rt = state.gse.lock().unwrap();
    rt.clock -= std::time::Duration::from_millis(advance_ms);
    (
        rt.operational.qd.status(rt.now()),
        rt.operational.qd_dispatch_utc_ms.unwrap(),
    )
}

#[cfg(test)]
mod mission_calibration_guard_tests {
    use super::*;
    #[test]
    fn imported_calibration_waits_for_idle_and_explicit_qualification_reset() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut runtime = Runtime::default();
        assert!(runtime.calibration_import_allowed());
        runtime.engine.status.phase = Phase::BaselineSetup;
        assert!(!runtime.calibration_import_allowed());
        runtime.engine.status.phase = Phase::Idle;
        let generation = runtime.operational.saved.qualification.generation();
        runtime
            .operational
            .saved
            .qualification
            .complete(Milestone::SelfTest, generation)
            .unwrap();
        assert!(!runtime.calibration_import_allowed());
        runtime.operational.saved.qualification.reset().unwrap();
        assert!(runtime.calibration_import_allowed());
    }
    #[test]
    fn hitl_does_not_import_paired_calibration() {
        crate::mode_suite!(Hitl);
        assert!(!Runtime::default().calibration_import_allowed());
    }
}

#[cfg(test)]
mod valve_capture_tests;

/// Lock-free producer snapshot: sequence producers may hold their own mutex.
/// Only a verified explicit Startup reset may advance this value.
/// Nonwaiting mission-phase probe: whether fill work is running or propellant
/// is loaded. Covers the nitrogen test (including its passed pressure hold),
/// fill and pause, turbocharge and cold flow holds. `None` when the GSE owner
/// is busy; the caller keeps its last observation.
pub(crate) fn mission_fill_activity(state: &AppState) -> Option<bool> {
    let rt = state.gse.try_lock().ok()?;
    Some(runtime_fill_activity(&rt))
}

fn runtime_fill_activity(rt: &Runtime) -> bool {
    let engine = matches!(
        rt.engine.status.phase,
        Phase::NitrogenSetup
            | Phase::Raising
            | Phase::Settling
            | Phase::Holding
            | Phase::Dumping
            | Phase::Passed
            | Phase::FillSetup
            | Phase::Filling
            | Phase::PauseSetup
            | Phase::Paused
            | Phase::Turbocharge
    );
    let loaded = !crate::runtime_mode::is_hitl()
        && matches!(
            rt.operational.saved.qualification.stage(),
            gse_sequence::qualification::MissionStage::Fill
                | gse_sequence::qualification::MissionStage::Armed
        );
    engine || loaded || rt.turbocharge.is_some() || cold_flow::runtime_active(rt)
}

pub(crate) fn automatic_generation(state: &AppState) -> u64 {
    state
        .automatic_generation
        .load(std::sync::atomic::Ordering::Acquire)
}

/// Calibration publication is bounded SDK admission, never calibration file IO.
pub(crate) fn sync_calibration_after_automatic_zero(
    state: &AppState,
    generation: u64,
    calibration: &crate::loadcell::LoadcellCalibrationFile,
) -> Result<(), String> {
    let _dispatch = dispatch_gate();
    let Some(_claim) = claim_operator_dispatch_under_gate(state, generation) else {
        return Err("Automatic zero cancelled by closed admission".into());
    };
    if matches!(generation, 0 | u64::MAX)
        || generation != automatic_generation(state)
        || manual_stop_pending(state)
    {
        return Err("Automatic zero cancelled by reset/manual Stop".into());
    }
    if let Some(router) = state.topology_router.get() {
        crate::network_variables::set_daq_calibration(router, calibration)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn queue_automatic_abort_for_generation(
    state: &Arc<AppState>,
    router: &sedsnet::router::Router,
    generation: u64,
    reason: &str,
) -> sedsnet::TelemetryResult<bool> {
    let _dispatch = dispatch_gate();
    let Some(_claim) = claim_operator_dispatch_under_gate(state, generation) else {
        return Ok(false);
    };
    if generation != automatic_generation(state) || !automatic_abort_allowed(state) {
        return Ok(false);
    }
    crate::telemetry_task::queue_automatic_abort_packet(
        router,
        reason,
        crate::abort_retirement::AbortCause::Unknown(reason.to_string()),
    )?;
    Ok(true)
}

/// Admission bookkeeping shares the epoch gate with reset publication. A reset
/// coordinator must clear ordinary dedup state under this SAME gate before
/// reopening admission, so an older request cannot repopulate the new epoch.
/// Authenticate and check command permissions first; manual Stop uses its
/// independent retained-intent path before ordinary admission.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OperatorCommandAdmission {
    Stale,
    Duplicate,
    Fresh,
}

#[cfg(test)]
pub(crate) fn record_operator_command(
    state: &AppState,
    generation: u64,
    command: &TelemetryCommand,
    now_ms: u64,
    duplicate_window_ms: u64,
) -> OperatorCommandAdmission {
    record_operator_command_with_launch_origin(
        state,
        generation,
        command,
        now_ms,
        duplicate_window_ms,
        None,
    )
    .unwrap_or(OperatorCommandAdmission::Stale)
}

/// A new validated launch revision retires only launch dedup keys. Keep the
/// board gate through this RAM-only update so Abort cannot race bookkeeping.
#[cfg(test)]
pub(crate) fn record_operator_command_with_launch_origin(
    state: &AppState,
    generation: u64,
    command: &TelemetryCommand,
    now_ms: u64,
    duplicate_window_ms: u64,
    launch_revision: Option<u64>,
) -> Result<OperatorCommandAdmission, String> {
    record_operator_command_with_recovery_origin(state,generation,command,now_ms,duplicate_window_ms,launch_revision,&mut None)
}
#[cfg(test)]
pub(crate) fn record_operator_command_with_recovery_origin(
    state:&AppState, generation:u64, command:&TelemetryCommand, now_ms:u64,
    duplicate_window_ms:u64, launch_revision:Option<u64>, recovery_origin:&mut Option<crate::operator_queue::manual_origin::Origin>,
)->Result<OperatorCommandAdmission,String> {
    record_operator_command_with_recovery_origin_diagnostic(state,generation,command,now_ms,duplicate_window_ms,launch_revision,RecoveryAdmissionDiagnostic { origin: recovery_origin, rejection: &mut None })
}
/// Borrow the original recovery claim and its rejection output together;
/// neither field captures a replacement command origin.
pub(crate) struct RecoveryAdmissionDiagnostic<'a> {
    pub origin: &'a mut Option<crate::operator_queue::manual_origin::Origin>,
    pub rejection: &'a mut Option<crate::operator_queue::manual_origin::Rejection>,
}
pub(crate) fn record_operator_command_with_recovery_origin_diagnostic(
    state:&AppState, generation:u64, command:&TelemetryCommand, now_ms:u64,
    duplicate_window_ms:u64, launch_revision:Option<u64>, diagnostic:RecoveryAdmissionDiagnostic<'_>,
)->Result<OperatorCommandAdmission,String> {
    let RecoveryAdmissionDiagnostic { origin: recovery_origin, rejection } = diagnostic;
    let _dispatch = dispatch_gate();
    // A second copy must not spend a relief epoch then disappear as Duplicate:
    // that would invalidate the only queued real command. Check duplicates
    // under the same owner/generation/Stop/deadline context, without mutation.
    if generation==automatic_generation(state) && recovery_origin.as_ref().is_some_and(|origin|origin.same_command_context(state,command))
        && state.software_command_is_duplicate(command,now_ms,duplicate_window_ms) {
        return Ok(OperatorCommandAdmission::Duplicate);
    }
    let recovery_current = recovery_origin.as_ref().is_some_and(|origin| origin.admits(state,command));
    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) || generation != automatic_generation(state)
        || (state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0 && !recovery_current)
    {
        use crate::operator_queue::manual_origin::Rejection;
        *rejection=Some(if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {Rejection::Shutdown}
            else if generation==0 {Rejection::MissingOrigin}
            else if generation!=automatic_generation(state) {Rejection::ResetChanged}
            else {recovery_origin.as_ref().map_or(Rejection::MissingOrigin,|origin|origin.rejection(state))});
        return Ok(OperatorCommandAdmission::Stale);
    }
    if let Some(origin)=recovery_origin.as_mut()
        && (!recovery_current || !origin.admit(state)) {*rejection=Some(origin.rejection(state));return Ok(OperatorCommandAdmission::Stale);}
    let record = || {
        if state.record_software_command_if_fresh(command, now_ms, duplicate_window_ms) {
            OperatorCommandAdmission::Fresh
        } else {
            OperatorCommandAdmission::Duplicate
        }
    };
    if crate::operator_phase::is_local(command) || !crate::board_abort::launch_command(crate::sequences::command_name(command)) {
        return Ok(record());
    }
    let revision = crate::board_abort::validate_launch_revision(state, launch_revision)?;
    state.board_abort.admit_launch(
        state,
        crate::board_abort::required_mask(state),
        revision,
        || {
            if state
                .launch_dedup_revision
                .load(std::sync::atomic::Ordering::Acquire)
                != revision
            {
                let mut history = state.last_command_ms.lock().unwrap();
                for name in [
                    "Launch",
                    "GroundStationLaunch",
                    "AdvanceFlightState",
                    "RewindFlightState",
                ] {
                    history.remove(&format!("dedup:{name}"));
                }
                state
                    .launch_dedup_revision
                    .store(revision, std::sync::atomic::Ordering::Release);
            }
            record()
        },
    )
}

#[cfg(test)]
mod operator_admission_tests {
    use super::*;

    #[tokio::test]
    async fn ordinary_epoch_admission_preserves_duplicates_and_reset_reopens_same_command() {
        let state = crate::state::tests::test_app_state().await;
        let generation = automatic_generation(&state);
        let command = TelemetryCommand::Postinit;
        assert_eq!(
            record_operator_command(&state, generation, &command, 10_000, 500),
            OperatorCommandAdmission::Fresh
        );
        assert_eq!(
            record_operator_command(&state, generation, &command, 10_001, 500),
            OperatorCommandAdmission::Duplicate
        );
        // Model only the coordinator's in-memory publication boundary: no
        // physical reset or board proof is authorized by this test helper.
        {
            let _dispatch = dispatch_gate();
            state.last_command_ms.lock().unwrap().clear();
            state
                .automatic_generation
                .store(generation + 1, std::sync::atomic::Ordering::Release);
        }
        assert_eq!(
            record_operator_command(&state, generation, &command, 10_002, 500),
            OperatorCommandAdmission::Stale
        );
        assert!(state.last_command_ms.lock().unwrap().is_empty());
        assert_eq!(
            record_operator_command(&state, generation + 1, &command, 10_003, 500),
            OperatorCommandAdmission::Fresh
        );
    }

    #[tokio::test]
    async fn hitl_board_proof_optional_actual_sdk_launch_absent_boards_and_known_abort_always_blocks()
     {
        crate::mode_suite!(Hitl);
        if crate::operator_queue::isolated_native_case(module_path!(),"hitl_board_proof_optional_actual_sdk_launch_absent_boards_and_known_abort_always_blocks"){return;}
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = crate::state::tests::test_app_state().await;
        state.av_bay_comms_connected.store(true, Ordering::Relaxed);
        state.fill_comms_connected.store(true, Ordering::Relaxed);
        let owners = Arc::new(AtomicUsize::new(0));
        let received = owners.clone();
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([
                crate::ignition_pair::fixture::handler(&state, |_|Ok(())),
                sedsnet::router::EndpointHandler::new_packet_handler(
                    crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                    move |packet| {
                        assert_eq!(
                            packet.payload(),
                            &[crate::rocket_commands::FlightComputerCommands::Launch as u8]
                        );
                        received.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                ),
            ])
            .with_sender("GS"),
        ));
        state.topology_router.set(router.clone()).unwrap();
        crate::telemetry_task::refresh_host_network_time(
            &router,
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        let kind = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
        let payload = [crate::rocket_commands::FlightComputerCommands::Launch as u8];
        assert!(!state.board_state_proof_enforced());
        assert_eq!(crate::board_abort::required_mask(&state), 0);
        // With the HITL proof interlock disabled, absent board proof is optional.
        // Verify an actual SDK owner and callback, not merely queue acceptance.
        let missing = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert!(queue_controller_with_launch_revision(&state,&router,kind,&payload,missing).unwrap());
        assert!(router.has_pending_reliable_tx_type(kind));
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(owners.load(Ordering::SeqCst),1);
        // Genuine queried AB/VB peers on this exact Arc Router; all other five
        // boards stay absent. No synthetic proof mask or board-status seed.
        let _actual_pair=crate::ignition_pair::fixture::Peers::warm_pair_only(&state,router.clone()).await;
        let(aborted,proven)=state.board_abort.masks();
        assert_eq!(aborted,0);assert_eq!(proven,crate::ignition_pair::REQUIRED_MASK);
        assert_eq!(crate::board_abort::required_mask(&state),0);
        let old = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert!(
            queue_controller_with_launch_revision(&state, &router, kind, &payload, old).unwrap()
        );
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(owners.load(Ordering::SeqCst), 2);
        state.toggle_hitl_safety_interlock(3);
        assert!(state.board_state_proof_enforced());
        // With the proof interlock on, a board on the network without a fresh
        // proof blocks Launch; absent boards would not (HITL bench rule).
        state.mark_board_seen("FC", crate::telemetry_task::get_current_timestamp_ms());
        assert!(
            !queue_controller_with_launch_revision(
                &state,
                &router,
                kind,
                &payload,
                crate::board_abort::capture_launch_revision_named(&state, "Launch")
            )
            .unwrap()
        );
        state.toggle_hitl_safety_interlock(3);
        assert!(
            !queue_controller_with_launch_revision(&state, &router, kind, &payload, old).unwrap(),
            "old intent does not gain authority after off/on policy transitions"
        );
        state.board_abort.observe_abort(
            crate::types::Board::FlightComputer,
            Instant::now(),
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        assert!(
            !queue_controller_with_launch_revision(
                &state,
                &router,
                kind,
                &payload,
                crate::board_abort::capture_launch_revision_named(&state, "Launch")
            )
            .unwrap()
        );
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(owners.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn board_abort_launch_dedup_uses_original_revision_and_preserves_unrelated_history() {
        let state = crate::state::tests::test_app_state().await;
        let generation = automatic_generation(&state);
        let prove = || {
            for board in crate::board_abort::BOARDS {
                let at = Instant::now();
                state.board_abort.observe_status(
                    board,
                    42,
                    8,
                    1,
                    Some(1),
                    false,
                    true,
                    at,
                    state.board_abort.query_revision(board),
                    at,
                    1,
                );
            }
        };
        // Model only fresh correlated readbacks here; the actual SDK matrix
        // independently exercises query nonce/issuer validation.
        prove();
        let original = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_eq!(
            record_operator_command_with_launch_origin(
                &state,
                generation,
                &TelemetryCommand::Launch,
                10_000,
                500,
                original
            )
            .unwrap(),
            OperatorCommandAdmission::Fresh
        );
        state
            .last_command_ms
            .lock()
            .unwrap()
            .insert("dedup:Postinit".into(), 9999);
        state
            .last_command_ms
            .lock()
            .unwrap()
            .insert("Launch".into(), 9998);
        state
            .board_abort
            .observe_abort(crate::types::Board::ActuatorBoard, Instant::now(), 10_001);
        prove();
        let before = state.last_command_ms.lock().unwrap().clone();
        assert!(
            record_operator_command_with_launch_origin(
                &state,
                generation,
                &TelemetryCommand::Launch,
                10_001,
                500,
                original
            )
            .is_err()
        );
        assert_eq!(*state.last_command_ms.lock().unwrap(), before);
        let fresh = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_ne!(fresh, original);
        assert_eq!(
            record_operator_command_with_launch_origin(
                &state,
                generation,
                &TelemetryCommand::Launch,
                10_002,
                500,
                fresh
            )
            .unwrap(),
            OperatorCommandAdmission::Fresh
        );
        let history = state.last_command_ms.lock().unwrap();
        assert_eq!(history.get("dedup:Postinit"), Some(&9999));
        assert_eq!(history.get("Launch"), Some(&9998));
        assert_eq!(history.get("dedup:Launch"), Some(&10_002));
    }

    #[tokio::test]
    async fn stale_epoch_does_not_refresh_existing_duplicate_entry() {
        let state = crate::state::tests::test_app_state().await;
        let generation = automatic_generation(&state);
        assert_eq!(
            record_operator_command(&state, generation, &TelemetryCommand::Postinit, 10_000, 500),
            OperatorCommandAdmission::Fresh
        );
        let before = state.last_command_ms.lock().unwrap().clone();
        assert_eq!(
            record_operator_command(
                &state,
                generation + 1,
                &TelemetryCommand::Postinit,
                11_000,
                500
            ),
            OperatorCommandAdmission::Stale
        );
        assert_eq!(*state.last_command_ms.lock().unwrap(), before);
    }
}

/// A generation-checked ordinary command owns its dispatch until this lease
/// drops; reset must quiesce these owners before publishing any board floor.
pub(crate) struct OperatorDispatchClaim(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl Drop for OperatorDispatchClaim {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}
/// Capture before authentication/IO awaits; closed admission has no origin.
pub(crate) fn capture_operator_origin(state: &AppState) -> Option<u64> {
    let _dispatch = dispatch_gate();
    (!state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) && state
        .operator_admission_barrier
        .load(std::sync::atomic::Ordering::Acquire)
        == 0)
        .then(|| automatic_generation(state))
}
pub(crate) fn claim_operator_dispatch(
    state: &AppState,
    generation: u64,
) -> Option<OperatorDispatchClaim> {
    let _dispatch = dispatch_gate();
    claim_operator_dispatch_under_gate(state, generation)
}
/// Only this RAM flag is published under the dispatch gate; file IO remains
/// entirely in the settings worker after this helper returns.
pub(crate) fn claim_plumbing_settings_dispatch(
    state: &AppState,
    generation: u64,
) -> Option<OperatorDispatchClaim> {
    let _dispatch = dispatch_gate();
    let claim = claim_operator_dispatch_under_gate(state, generation)?;
    if state.plumbing_drive.motion_worker_pending() {
        return None;
    }
    state.board_abort.change_launch_context_if(|| {
        state.plumbing_drive.begin_settings();
        true
    });
    Some(claim)
}
fn claim_operator_dispatch_under_gate(
    state: &AppState,
    generation: u64,
) -> Option<OperatorDispatchClaim> {
    if generation != automatic_generation(state)
        || state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        #[cfg(all(test, not(feature = "firmware_sim")))]
        if !crate::runtime_mode::is_hitl_or_test_fire() {
            crate::normal_publication_probe::claim_refused(state, generation);
        }
        return None;
    }
    claim_operator_inflight_under_gate(state)
}
fn claim_operator_inflight_under_gate(state: &AppState) -> Option<OperatorDispatchClaim> {
    let count = &state.operator_dispatch_inflight;
    let mut current = count.load(std::sync::atomic::Ordering::Acquire);
    loop {
        let next = current.checked_add(1)?;
        match count.compare_exchange_weak(
            current,
            next,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                let claim = OperatorDispatchClaim(count.clone());
                if state
                    .shutdown_requested
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    drop(claim);
                    return None;
                }
                return Some(claim);
            }
            Err(observed) => current = observed,
        }
    }
}

/// A sealed safe direction may cross a retained recovery barrier without
/// reopening it. Retire the coordinator's original quiescence proof first.
#[cfg(test)]
pub(crate) fn admit_safe_manual<T>(
    state: &AppState,
    intent: &crate::operator_queue::safe_manual::Intent,
    offer: impl FnOnce() -> T,
) -> Option<(T, OperatorDispatchClaim)> {
    admit_safe_manual_unless_unadmitted(state, intent, offer, |_| false)
}
/// `unadmitted` marks a result the SDK never took ownership of (its core was
/// held). That offer leaves the relief epoch where it was, so the caller may
/// offer the same intent again.
pub(crate) fn admit_safe_manual_unless_unadmitted<T>(
    state: &AppState,
    intent: &crate::operator_queue::safe_manual::Intent,
    offer: impl FnOnce() -> T,
    unadmitted: impl Fn(&T) -> bool,
) -> Option<(T, OperatorDispatchClaim)> {
    use std::sync::atomic::Ordering;
    let _dispatch = dispatch_gate();
    if !intent.current(state) || state.operator_queue.fill_launch_active() || !state.action_policy.lock().unwrap().software_buttons_enabled {
        return None;
    }
    let token = intent.token();
    state.manual_abort_intent.admit_safe_if_epoch_unchanged(token.manual_epoch, || {
        state.deferred_abort.admit_safe_if_epoch_unchanged(token.observed_manual_epoch, || {
            let count = &state.operator_dispatch_inflight;
            let mut current = count.load(Ordering::Acquire);
            loop {
                match count.compare_exchange_weak(current, current.checked_add(1)?, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
            let claim = OperatorDispatchClaim(count.clone());
            if !state.operator_queue.advance_relief_epoch(token.relief_epoch) { return None; }
            let result = offer();
            if unadmitted(&result) {
                state.operator_queue.restore_relief_epoch(token.relief_epoch);
            }
            Some((result, claim))
        }).flatten()
    }).flatten()
}

pub(crate) fn claim_manual_dispatch(state:&AppState,generation:u64,origin:Option<crate::operator_queue::manual_origin::Origin>)->Option<OperatorDispatchClaim> {
    let _dispatch=dispatch_gate();
    if let Some(origin)=origin {
        if origin.generation()!=generation || !origin.dispatched(state) {return None;}
        claim_operator_inflight_under_gate(state)
    } else {claim_operator_dispatch_under_gate(state,generation)}
}

/// The SDK callback never takes these locks. Marked automatic observations must
/// pass the same current launch/reset boundary as automatic command producers.
/// Explicit local human Stop, ALREADY owned by SDK. No generic received Abort
/// can manufacture this receipt. Only bounded RAM changes occur under guards.
pub(crate) fn commit_local_manual_stop(
    state: &AppState,
    claim: &crate::manual_abort_intent::Claim,
    receipt: &crate::manual_abort_utc::LocalStopAdmission,
) -> bool {
    let _dispatch = dispatch_gate();
    if matches!(receipt.generation(), 0 | u64::MAX)
        || receipt.generation() != automatic_generation(state)
        || receipt.epoch() != claim.epoch()
        || state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return false;
    }
    claim.with_current_publication(&state.manual_abort_intent, receipt.wire(), || {
        let mut current = state.state.lock().unwrap();
        *current = FlightState::Aborted;
        state.clear_launch_sequence_command_pending();
        state.launch_mode_gate.clear_sent();
        state.set_abort_indicator_latched(true);
    }).is_some()
}

pub(crate) fn accept_observed_abort(state: &AppState, automatic: bool, generation: u64) -> bool {
    let _dispatch = dispatch_gate();
    if automatic && (generation != automatic_generation(state) || !automatic_abort_allowed(state)) {
        return false;
    }
    state.clear_launch_sequence_command_pending();
    state.launch_mode_gate.clear_sent();
    state.set_abort_indicator_latched(true);
    true
}

#[cfg(test)]
mod recovery_generation_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn operator_claims_reject_old_tokens_and_runtime_replacement_cannot_rewind_generation() {
        let state = crate::state::tests::test_app_state().await;
        let first = automatic_generation(&state);
        let claim = claim_operator_dispatch(&state, first).unwrap();
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 1);
        drop(claim);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
        state
            .automatic_generation
            .store(first + 1, Ordering::Release);
        *state.gse.lock().unwrap() = Runtime::default();
        assert_eq!(automatic_generation(&state), first + 1);
        assert!(claim_operator_dispatch(&state, first).is_none());
        assert!(claim_operator_dispatch(&state, first + 1).is_some());
        state
            .operator_dispatch_inflight
            .store(usize::MAX, Ordering::Release);
        assert!(claim_operator_dispatch(&state, first + 1).is_none());
        state.operator_dispatch_inflight.store(0, Ordering::Release);
        for command in [
            crate::types::TelemetryCommand::Postinit,
            crate::types::TelemetryCommand::Launch,
        ] {
            assert!(
                crate::telemetry_task::command_at_dispatch(
                    &state,
                    crate::types::TelemetryCommand::QueuedOperator {
            launch_hold: None,
            recovery_origin: None,
                        launch_revision: None,
                        plumbing_origin: None,
                        generation: first,
                        command: Box::new(command)
                    }
                )
                .is_none()
            );
        }
    }

    #[tokio::test]
    async fn launch_refuses_actual_sdk_controller_owners_before_journal_commit() {
        crate::mode_suite!(Normal, TestFire, Testing);
        use sedsnet::router::{EndpointHandler, Router, RouterConfig};
        for name in ["ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND"] {
            let state = crate::state::tests::test_app_state().await;
            qualified_launch_proofs_for_test(&state);
            let endpoint = crate::telemetry_schema::endpoint("ACTUATOR_BOARD");
            let router = Arc::new(Router::new(RouterConfig::new([
                EndpointHandler::new_packet_handler(endpoint, |_| Ok(())),
            ])));
            state.topology_router.set(router.clone()).unwrap();
            router
                .tx_queue(
                    sedsnet::packet::Packet::new(
                        crate::telemetry_schema::data_type(name),
                        &[endpoint],
                        "GS",
                        100,
                        Arc::from(if name == "ABORT" {
                            &b"manual"[..]
                        } else {
                            &b"\x00"[..]
                        }),
                    )
                    .unwrap(),
                )
                .unwrap();
            assert!(
                controller_delivery_pending(&state),
                "{name} queue must own delivery"
            );
            let result = inhibit_automatic_actions_for_launch(&state);
            assert!(result.unwrap_err().contains("Controller command delivery"));
            if !crate::runtime_mode::is_hitl() {
                assert!(
                    commit_fire_intent(&state)
                        .unwrap_err()
                        .contains("Controller command delivery")
                );
            }
            assert!(
                !state
                    .launch_history
                    .lock()
                    .unwrap()
                    .automatic_actions_inhibited()
            );
            router.process_all_queues_with_timeout(0).unwrap();
            assert!(
                !controller_delivery_pending(&state),
                "local delivery must retire {name}"
            );
            // Retirement removes this prerequisite; the existing independent
            // state/interlock policy still decides whether real Fire is legal.
            assert!(inhibit_automatic_actions_for_launch(&state).is_ok());
        }
    }
}

#[cfg(test)]
mod final_launch_admission_tests {
    use super::*;
    use sedsnet::packet::Packet;
    use sedsnet::router::{Router, RouterConfig};
    fn fixture_state_proofs(state: &AppState) {
        // Modeled authoritative readbacks isolate these pre-existing Stop/SDK
        // ownership tests. The board_abort actual-SDK matrix tests the real
        // nonce/issuer transport path separately, without this fixture.
        for board in crate::board_abort::BOARDS {
            let at = Instant::now();
            let revision = state.board_abort.query_revision(board);
            state.board_abort.observe_status(
                board,
                42,
                8,
                1,
                Some(1),
                false,
                true,
                at,
                revision,
                at,
                1,
            );
        }
    }
    fn clocked_router(config: RouterConfig) -> Router {
        let router = Router::new(config);
        crate::telemetry_task::refresh_host_network_time(
            &router,
            crate::telemetry_task::get_current_timestamp_ms(),
        );
        router
    }
    fn launch_packet() -> Packet {
        Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            ],
            "GS",
            100,
            Arc::from(&[1u8][..]),
        )
        .unwrap()
    }
    fn manual_packet() -> Packet {
        Packet::new(
            crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("ABORT")],
            "GS",
            100,
            Arc::from(&b"manual"[..]),
        )
        .unwrap()
    }
    #[tokio::test]
    async fn board_abort_queued_launch_origin_cannot_refresh_after_abort_and_fresh_clear() {
        if crate::operator_queue::isolated_native_case(module_path!(),"board_abort_queued_launch_origin_cannot_refresh_after_abort_and_fresh_clear"){return;}
        let state = crate::state::tests::test_app_state().await;
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capture = received.clone();
        let router = Arc::new(clocked_router(RouterConfig::new([crate::ignition_pair::fixture::handler(
            &state,
            move |_| {
                capture.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )])));
        let mut actual_peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        let generation = automatic_generation(&state);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(TelemetryCommand::QueuedOperator {
            launch_hold: None,
            recovery_origin: None,
            generation,
            command: Box::new(TelemetryCommand::Launch),
            plumbing_origin: None,
            launch_revision: revision,
        })
        .await
        .unwrap();
        state
            .board_abort
            .observe_abort(crate::types::Board::ActuatorBoard, Instant::now(), 10_000);
        actual_peers.refresh(&state).await;
        let TelemetryCommand::QueuedOperator {
            launch_revision, ..
        } = rx.recv().await.unwrap()
        else {
            panic!("real application queue must retain its original envelope")
        };
        let kind = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
        let old_accepted =
            queue_controller_with_launch_revision(&state, &router, kind, &[1], launch_revision)
                .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(
            received.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "old queued intent acquired actual SDK owner delivery"
        );
        assert!(!old_accepted);
        let fresh = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_ne!(fresh, launch_revision);
        assert!(queue_controller_with_launch_revision(&state, &router, kind, &[1], fresh).unwrap());
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn board_abort_queued_launch_origin_cannot_refresh_after_stop_without_board_abort() {
        if crate::operator_queue::isolated_native_case(module_path!(),"board_abort_queued_launch_origin_cannot_refresh_after_stop_without_board_abort"){return;}
        let state = crate::state::tests::test_app_state().await;
        let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let capture = received.clone();
        let router = Arc::new(clocked_router(RouterConfig::new([crate::ignition_pair::fixture::handler(
            &state,
            move |_| {
                capture.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        )])));
        let _actual_peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        let generation = automatic_generation(&state);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(TelemetryCommand::QueuedOperator {
            launch_hold: None,
            recovery_origin: None,
            generation,
            command: Box::new(TelemetryCommand::Launch),
            plumbing_origin: None,
            launch_revision: revision,
        })
        .await
        .unwrap();
        state.manual_abort_intent.request_untimed();
        // Model SDK ownership retirement, not physical recovery. No board
        // Abort report is introduced: the retained Stop epoch alone fences it.
        state.manual_abort_intent.try_claim().unwrap().accepted();
        assert_eq!(state.board_abort.masks().0, 0);
        crate::board_abort::synchronize_stop_revision(&state);
        let TelemetryCommand::QueuedOperator {
            launch_revision, ..
        } = rx.recv().await.unwrap()
        else {
            panic!("real application queue must retain its original envelope")
        };
        let kind = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
        let old_accepted =
            queue_controller_with_launch_revision(&state, &router, kind, &[1], launch_revision)
                .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(
            received.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "old queued intent acquired actual SDK owner delivery"
        );
        assert!(!old_accepted);
        let fresh = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_ne!(fresh, launch_revision);
        assert!(queue_controller_with_launch_revision(&state, &router, kind, &[1], fresh).unwrap());
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(received.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn exhausted_stop_epochs_never_certify_unchanged_launch_admission() {
        for deferred in [false, true] {
            let state = crate::state::tests::test_app_state().await;
            fixture_state_proofs(&state);
            if deferred {
                state.deferred_abort.exhaust_manual_epoch_for_test();
            } else {
                state.manual_abort_intent.exhaust_epoch_for_test();
            }
            assert!(
                inhibit_automatic_actions_for_launch(&state)
                    .unwrap_err()
                    .contains("epoch exhausted")
            );
            if !crate::runtime_mode::is_hitl() {
                assert!(
                    commit_fire_intent(&state)
                        .unwrap_err()
                        .contains("epoch exhausted")
                );
            }
            assert!(
                !state
                    .launch_history
                    .lock()
                    .unwrap()
                    .automatic_actions_inhibited()
            );
            let router = clocked_router(RouterConfig::new([]));
            assert!(
                queue_admitted_launch_packets(
                    &state,
                    &router,
                    LaunchAdmission::capture(&state),
                    vec![launch_packet()]
                )
                .unwrap_err()
                .contains("epoch exhausted")
            );
            assert!(
                !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                    "FLIGHT_COMMAND"
                ))
            );
            if !deferred {
                assert_eq!(state.manual_abort_intent.request(), u64::MAX);
                assert!(state.manual_abort_intent.pending_epoch().is_some());
            }
        }
    }
    /// HITL "no way out" paths: none of these can veto a fresh press when no
    /// board on the network reports Abort.
    #[tokio::test]
    async fn hitl_fresh_press_is_not_held_by_undeliverable_state() {
        crate::mode_suite!(Hitl);
        let stopped = crate::state::tests::test_app_state().await;
        let router = Arc::new(clocked_router(RouterConfig::new([])));
        stopped.topology_router.set(router.clone()).unwrap();
        stopped.set_abort_indicator_latched(true);
        let revision = crate::board_abort::capture_launch_revision_named(&stopped, "Launch");
        assert!(inhibit_automatic_actions_for_launch_revision(&stopped, revision).is_ok(),
            "a Stop that never reached an absent board must not hold a fresh press");
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(clocked_router(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        for name in ["ABORT", "VALVE_COMMAND", "ACTUATOR_COMMAND", "FLIGHT_COMMAND"] {
            let kind = crate::telemetry_schema::data_type(name);
            let payload: &[u8] = if name == "ABORT" { b"manual" } else { b"\x00" };
            router.tx_queue(Packet::new(kind, &[match name {
                "VALVE_COMMAND" => crate::telemetry_schema::endpoint("VALVE_BOARD"),
                "ACTUATOR_COMMAND" => crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                _ => crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            }], "GS", 100, Arc::from(payload)).unwrap()).unwrap();
        }
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert!(inhibit_automatic_actions_for_launch_revision(&state, revision).is_ok(),
            "commands retained for absent boards must not hold a fresh press");
    }
    /// Only the dedicated reset unlocks Launch; a T-clock reset must not.
    #[tokio::test]
    async fn t_clock_reset_does_not_unlock_launch() {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(clocked_router(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        crate::gse::qualified_launch_proofs_for_test(&state);
        state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        state.reset_launch_clock();
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_eq!(inhibit_automatic_actions_for_launch_revision(&state, revision).unwrap_err(), LAUNCH_LOCKED_MESSAGE);
    }
    /// Turbocharge is pre-fire: it must be finished or cancelled before Launch.
    #[tokio::test]
    async fn launch_is_refused_while_turbocharge_runs() {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(clocked_router(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        crate::gse::qualified_launch_proofs_for_test(&state);
        state.gse.lock().unwrap().engine.status.phase = Phase::Turbocharge;
        assert_eq!(command_allowed(&state, &TelemetryCommand::Launch), Some(false));
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert_eq!(inhibit_automatic_actions_for_launch_revision(&state, revision).unwrap_err(), TURBOCHARGE_UNFINISHED_MESSAGE);
        state.gse.lock().unwrap().engine.status.phase = Phase::Idle;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        assert!(inhibit_automatic_actions_for_launch_revision(&state, revision).is_ok());
    }
    /// A Stop pressed after Launch was pressed still cancels that Launch.
    #[tokio::test]
    async fn hitl_stop_pressed_after_launch_press_still_cancels_it() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(clocked_router(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        let ticket = inhibit_automatic_actions_for_launch_revision(&state, revision).unwrap();
        let _ = state.manual_abort_intent.request();
        assert!(queue_admitted_launch_packets(&state, &router, ticket, vec![launch_packet()]).is_err());
    }
    #[tokio::test]
    async fn retained_or_deferred_manual_abort_blocks_launch_before_sdk_admission() {
        crate::mode_suite!(Normal, TestFire, Testing);
        for deferred in [false, true] {
            let state = crate::state::tests::test_app_state().await;
            fixture_state_proofs(&state);
            let router = Arc::new(clocked_router(RouterConfig::new([])));
            state.topology_router.set(router.clone()).unwrap();
            if deferred {
                state
                    .deferred_abort
                    .observe(&manual_packet(), automatic_generation(&state));
            } else {
                state.manual_abort_intent.request();
            }
            let error = inhibit_automatic_actions_for_launch(&state).unwrap_err();
            eprintln!("STOP_PREPARATION_REFUSAL deferred={deferred} error={error}");
            assert_eq!(error, "Manual Abort invalidated launch admission; no launch effect was admitted");
            if !crate::runtime_mode::is_hitl() {
                assert_eq!(commit_fire_intent(&state).unwrap_err(), "Manual Abort invalidated launch admission; no launch effect was admitted");
            }
            for kind in ["ABORT", "FLIGHT_COMMAND", "VALVE_COMMAND", "ACTUATOR_COMMAND"] {
                assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(kind)), "Stop refusal must create no SDK owner: {kind}");
            }
            assert!(
                !state
                    .launch_history
                    .lock()
                    .unwrap()
                    .automatic_actions_inhibited()
            );
        }
    }
    #[tokio::test]
    async fn abort_between_durable_admission_and_final_sdk_queue_cannot_be_lost() {
        for deferred in [false, true] {
            let state = crate::state::tests::test_app_state().await;
            fixture_state_proofs(&state);
            let router = clocked_router(RouterConfig::new([]));
            let ticket = inhibit_automatic_actions_for_launch(&state).unwrap();
            if deferred {
                state
                    .deferred_abort
                    .observe(&manual_packet(), automatic_generation(&state));
            } else {
                state.manual_abort_intent.request();
            }
            assert!(
                queue_admitted_launch_packets(&state, &router, ticket, vec![launch_packet()])
                    .unwrap_err()
                    .contains("Manual Abort")
            );
            assert!(
                !router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                    "FLIGHT_COMMAND"
                ))
            );
            assert!(
                state
                    .launch_history
                    .lock()
                    .unwrap()
                    .automatic_actions_inhibited()
            );
        }
        // A request accepted and fully handed off between the two checks must
        // still invalidate the old epoch even when its pending bit is clear.
        let state = crate::state::tests::test_app_state().await;
        fixture_state_proofs(&state);
        let router = clocked_router(RouterConfig::new([]));
        let ticket = inhibit_automatic_actions_for_launch(&state).unwrap();
        state.manual_abort_intent.request();
        state.manual_abort_intent.try_claim().unwrap().accepted();
        assert!(state.manual_abort_intent.pending_epoch().is_none());
        assert!(
            queue_admitted_launch_packets(&state, &router, ticket, vec![launch_packet()]).is_err()
        );
    }
    #[tokio::test]
    async fn journal_wait_does_not_block_new_manual_intent_and_final_queue_rechecks_it() {
        let state = crate::state::tests::test_app_state().await;
        fixture_state_proofs(&state);
        let journal = state.launch_history.lock().unwrap();
        let worker_state = state.clone();
        let worker =
            std::thread::spawn(move || inhibit_automatic_actions_for_launch(&worker_state));
        // Generous bound: a loaded host can take well over a second to reach
        // the wait. Never panic while holding the journal: unwinding would
        // poison it and the worker would then panic on it.
        let limit = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if state.gse.try_lock().is_err() {
                break;
            }
            if worker.is_finished() || std::time::Instant::now() >= limit {
                drop(journal);
                panic!(
                    "launch must reach the actual journal wait: {:?}",
                    worker.join().map(|result| result.map(|_| ()))
                );
            }
            std::thread::yield_now();
        }
        // The real launch function holds GSE and waits on the held journal.
        // Stop admission remains an immediate independent RAM operation.
        assert_eq!(state.manual_abort_intent.request(), 1);
        drop(journal);
        let ticket = worker.join().unwrap().unwrap();
        let router = clocked_router(RouterConfig::new([]));
        assert!(
            queue_admitted_launch_packets(&state, &router, ticket, vec![launch_packet()]).is_err()
        );
        assert!(
            !router
                .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND"))
        );
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    #[tokio::test]
    async fn final_sdk_queue_never_executes_local_callbacks_or_flushes_under_intent_lock() {
        if crate::operator_queue::isolated_native_case(module_path!(),"final_sdk_queue_never_executes_local_callbacks_or_flushes_under_intent_lock"){return;}
        use std::sync::atomic::{AtomicUsize, Ordering};
        let state = crate::state::tests::test_app_state().await;
        let local = Arc::new(AtomicUsize::new(0));
        let seen = local.clone();
        let router = Arc::new(clocked_router(RouterConfig::new([crate::ignition_pair::fixture::handler(
            &state,
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )])));
        let _actual_peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
        let ticket = inhibit_automatic_actions_for_launch(&state).unwrap();
        assert_eq!(
            queue_admitted_launch_packets(&state, &router, ticket, vec![launch_packet()]).unwrap(),
            if crate::runtime_mode::is_hitl() { 1 } else { 2 }
        );
        assert_eq!(
            local.load(Ordering::SeqCst),
            0,
            "tx_queue must remain queue-only"
        );
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(
            local.load(Ordering::SeqCst),
            1,
            "only the controller packet targets GROUND_STATION; Launch7 targets FLIGHT_STATE"
        );
        assert_eq!(
            state.manual_abort_intent.request(),
            1,
            "fresh Stop after linearization remains accepted"
        );
    }
    #[tokio::test]
    async fn sdk_admission_failure_reports_actual_partial_ownership_without_retry_or_unlatching() {
        const NAME: &str = "gse::final_launch_admission_tests::sdk_admission_failure_reports_actual_partial_ownership_without_retry_or_unlatching";
        if std::env::var_os("GS_FINAL_LAUNCH_BUDGET_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("GS_FINAL_LAUNCH_BUDGET_CHILD", "1")
                .env("GS_SIM_MODE", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        crate::telemetry_schema::initialize().unwrap();
        // Keep the original empty-router SDK capacity boundary separately from
        // the admission integration, which now requires genuine same-router
        // discovery and queried board proof before any original offer.
        let make_router = |budget: usize| {
            let memory = sedsnet::config::RuntimeMemoryConfig::new(budget,1,budget.min(128),1.25).unwrap();
            clocked_router(RouterConfig::new([]).with_memory_config(memory).unwrap())
        };
        let fits=|budget|make_router(budget).tx_queue(launch_packet()).is_ok();
        let(mut lower,mut upper)=(16usize,16usize);
        while !fits(upper){lower=upper;upper*=2;assert!(upper<=1_048_576,"bounded fixture must admit one owner");}
        while upper-lower>1{let middle=lower+(upper-lower)/2;if fits(middle){upper=middle}else{lower=middle}}
        let valve=||Packet::new(crate::telemetry_schema::data_type("VALVE_COMMAND"),&[crate::telemetry_schema::endpoint("GROUND_STATION"),crate::telemetry_schema::endpoint("VALVE_BOARD")],"GS",100,Arc::from(&[crate::rocket_commands::ValveBoardCommands::Sequence as u8][..])).unwrap();
        let mut bare_partial=false;
        for budget in upper..upper+256 {let router=make_router(budget);if router.try_tx_queue(launch_packet()).is_ok()&&router.try_tx_queue(valve()).is_err(){
            assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
            assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));bare_partial=true;break;
        }}
        assert!(bare_partial,"the original bare SDK partial boundary remains exercised independently");
        let bare_zero=make_router(16);assert!(bare_zero.try_tx_queue(launch_packet()).is_err());assert!(!bare_zero.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
        let mut partial_seen=false;
        for budget in [18905usize,18957,19009] {
            let mut scene=crate::ignition_pair::constrained_fixture::Scene::new(budget).await;
            scene.warm().await.unwrap();scene.preload_readonly_gps_pressure();
            assert!(scene.scope_current(),"pair and required scope must remain current at original offer");
            let state=&scene.state;let router=&scene.host;
            let ticket=inhibit_automatic_actions_for_launch(state).unwrap();
            if let Err(message)=queue_prepared_launch_packets(state,router,ticket,vec![launch_packet(),valve()],None)
                && message.contains("accepted 1/2") {
                assert!(message.contains("manual recovery required"));
                assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
                assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
                assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
                assert_eq!(state.manual_abort_intent.epoch(),0,"queue refusal never automatically Aborts");
                partial_seen=true;
            }
            scene.state.request_shutdown();scene.state.telemetry_db_pool().close().await;scene.state.auth_db.close().await;
            if partial_seen{break;}
        }
        assert!(partial_seen,"actual SDK must retain the first admitted owner when second admission fails");
        // Additional readonly Ordered QUERY pressure is an actual retained
        // SDK owner; fresh proof remains valid and no queue is processed.
        let mut scene=crate::ignition_pair::constrained_fixture::Scene::new(18957).await;
        scene.warm().await.unwrap();scene.preload_readonly_gps_pressure();
        assert!(scene.preload_readonly_query(crate::types::Board::RFBoard));
        let state=&scene.state;let ticket=inhibit_automatic_actions_for_launch(state).unwrap();
        let refused=queue_prepared_launch_packets(state,&scene.host,ticket,vec![launch_packet()],None).unwrap_err();
        assert!(refused.contains("accepted 0/1"));assert!(refused.contains("queue saturated"));
        assert!(!scene.host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
        assert!(!scene.host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
        assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
        assert_eq!(state.manual_abort_intent.epoch(),0,"queue refusal never automatically Aborts");
        scene.state.request_shutdown();scene.state.telemetry_db_pool().close().await;scene.state.auth_db.close().await;

    }
}

#[cfg(test)]
mod retained_stop_fence_tests {
    use super::*;
    use sedsnet::router::{Router, RouterConfig};
    #[tokio::test]
    async fn active_fill_cannot_refill_control_capacity_after_manual_abort_sdk_refusal() {
        let state = crate::state::tests::test_app_state().await;
        let control = crate::state::tests::test_app_state().await;
        let router = Arc::new(Router::new(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        state.set_local_flight_state(FlightState::Idle);
        state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
        let kind = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
        let mut saturated = false;
        for _ in 0..10000 {
            if crate::telemetry_task::queue_controller_command(
                &router,
                kind,
                &[ActuatorBoardCommands::NitrogenOpen as u8],
            )
            .is_err()
            {
                saturated = true;
                break;
            }
        }
        assert!(
            saturated,
            "actual SDK queue must reach bounded backpressure"
        );
        state.manual_abort_intent.request();
        let claim = state.manual_abort_intent.try_claim().unwrap();
        assert!(crate::telemetry_task::queue_priority_manual_abort_packet(&router).is_err());
        drop(claim);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
        assert!(
            !state.abort_indicator_latched(),
            "retention is not observed/physical Abort confirmation"
        );
        // Test-only removal models a freed queue slot. Production does not
        // cancel/drop these accepted owners, nor falsely accept the Stop claim.
        router.clear_queues();
        assert!(
            queue_controller_with_stop_fence(
                &control,
                &router,
                kind,
                &[ActuatorBoardCommands::NitrogenOpen as u8]
            )
            .unwrap(),
            "the freed actual SDK capacity can admit the same opening without Stop"
        );
        router.clear_queues();
        apply(
            &state,
            Effects {
                valves: vec![(0, true), (3, true), (4, true)],
                ..Default::default()
            },
        );
        assert!(!router.has_pending_reliable_tx_type(kind));
        assert!(
            !router
                .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND"))
        );
        assert!(
            !queue_controller_with_stop_fence(
                &state,
                &router,
                kind,
                &[ActuatorBoardCommands::IgniterOn as u8]
            )
            .unwrap()
        );
        assert!(inhibit_automatic_actions_for_launch(&state).is_err());
        // Explicit abort-safe supply closes remain possible under Stop.
        assert!(
            queue_controller_with_stop_fence(
                &state,
                &router,
                kind,
                &[ActuatorBoardCommands::NitrogenClose as u8]
            )
            .unwrap()
        );
        assert!(router.has_pending_reliable_tx_type(kind));
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    #[tokio::test]
    async fn declined_testing_model_never_falls_through_and_sticky_inhibit_survives_visual_idle() {
        if crate::operator_queue::isolated_native_case(module_path!(),"declined_testing_model_never_falls_through_and_sticky_inhibit_survives_visual_idle"){return;}
        let state = crate::state::tests::test_app_state().await;
        let router=Arc::new(Router::new(RouterConfig::new([crate::ignition_pair::fixture::handler(&state,|_|Ok(()))]).with_sender("GS")));
        let _actual_peers=crate::ignition_pair::fixture::Peers::warm(&state,router).await;
        let ticket = inhibit_automatic_actions_for_launch(&state).unwrap();
        assert!(
            admit_simulated_launch(&state, ticket, || false)
                .unwrap_err()
                .contains("no physical fallback")
        );
        state.set_local_flight_state(FlightState::Idle);
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        assert!(
            crate::telemetry_task::command_at_dispatch(
                &state,
                TelemetryCommand::AutomaticAbort {
                    generation: automatic_generation(&state)
                }
            )
            .is_none()
        );
        assert!(inhibit_automatic_actions_for_launch(&state).is_err());
    }
}

#[cfg(test)]
mod explicit_fc_stop_tests {
    use super::*;
    #[tokio::test]
    async fn retained_stop_still_admits_exact_manual_fc_evaluation_abort_through_actual_sdk() {
        use sedsnet::router::{Router, RouterConfig};
        let state = crate::state::tests::test_app_state().await;
        state.manual_abort_intent.request();
        state.set_abort_indicator_latched(true);
        let router = Arc::new(Router::new(RouterConfig::new([])));
        state.topology_router.set(router.clone()).unwrap();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = sent.clone();
        router.add_side_packet("synthetic-flight-control", move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                capture.lock().unwrap().push(packet.payload().to_vec());
            }
            Ok(())
        });
        let kind = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
        let stop = crate::rocket_commands::FlightComputerCommands::EvaluationAbort as u8;
        assert_eq!(stop, 16);
        crate::telemetry_task::queue_locally_routed_flight_command(
            &state,
            &router,
            "explicit FC evaluation stop",
            &[stop],
        )
        .unwrap();
        assert!(router.has_pending_reliable_tx_type(kind));
        for forbidden in [1u8, 12, 13, 0x90] {
            assert!(
                !queue_controller_with_stop_fence(&state, &router, kind, &[forbidden]).unwrap()
            );
        }
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(sent.lock().unwrap().as_slice(), [vec![16]]);
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
}

#[cfg(test)]
mod pre_target_tests;

#[cfg(test)]
#[path = "gse/packed_owner_diagnostic.rs"]
mod packed_owner_diagnostic;

/// Receiver-owned administrative drain only: no SDK, IO, await or callback
/// outside this bounded queue operation. This does not reopen admission.
pub(crate) fn with_closed_operator_queue<T>(
    state: &AppState,
    token: crate::operator_queue::Token,
    drain: impl FnOnce() -> T,
) -> Option<T> {
    let _dispatch = dispatch_gate();
    if token.owner == 0
        || state
            .operator_admission_barrier
            .load(std::sync::atomic::Ordering::Acquire)
            != token.owner
        || automatic_generation(state) != token.generation
        || state.operator_queue.relief_epoch() != token.relief_epoch
        || state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    {
        return None;
    }
    state
        .manual_abort_intent
        .admit_if_unchanged(token.manual_epoch, || {
            state
                .deferred_abort
                .admit_if_no_manual_since(token.observed_manual_epoch, drain)
        })
        .flatten()
}

#[cfg(test)]
mod recovery_quiescence_tests {
    use super::*;
    #[tokio::test]
    async fn direct_hitl_tick_cannot_change_active_engine_while_admission_closed() {
        crate::mode_suite!(Hitl);
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().engine.status.phase = Phase::Filling;
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        tick(&state, false); // actual engine would fault on lost interlock
        let closed = state.gse.lock().unwrap().engine.status.phase;
        state
            .operator_admission_barrier
            .store(0, std::sync::atomic::Ordering::Release);
        tick(&state, false);
        assert_ne!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Filling
        );
        assert_eq!(closed, Phase::Filling);
    }
    #[tokio::test]
    async fn actual_report_safety_does_not_publish_old_effects_while_admission_closed() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let state = crate::state::tests::test_app_state().await;
        {
            let mut runtime = state.gse.lock().unwrap();
            runtime.engine.status.phase = Phase::Filling;
            runtime.observe_valve(ActuatorBoardCommands::NitrogenOpen as u8, true);
        }
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        report_safety(&state);
        let closed = state.gse.lock().unwrap().engine.status.phase;
        state
            .operator_admission_barrier
            .store(0, std::sync::atomic::Ordering::Release);
        report_safety(&state);
        assert_ne!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Filling
        );
        assert_eq!(closed, Phase::Filling);
    }
    #[tokio::test]
    async fn automatic_abort_shared_owner_is_fenced_and_lease_covers_latch_lifetime() {
        let state = crate::state::tests::test_app_state().await;
        state
            .operator_admission_barrier
            .store(77, std::sync::atomic::Ordering::Release);
        let closed_claim = claim_automatic_abort(&state);
        let closed_admitted = closed_claim.is_some();
        drop(closed_claim);
        // Restore only the fixture's automatic owner latch for the independent
        // fresh-positive control, not to make a stale command pass.
        *state.gse.lock().unwrap() = Runtime::default();
        state
            .operator_admission_barrier
            .store(0, std::sync::atomic::Ordering::Release);
        let fresh = claim_automatic_abort(&state).unwrap();
        let during = state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire);
        drop(fresh);
        assert!(!closed_admitted);
        assert_eq!(during, 1);
        assert_eq!(
            state
                .operator_dispatch_inflight
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        state.manual_abort_intent.request();
        assert!(state.manual_abort_intent.pending_epoch().is_some());
    }
    #[tokio::test]
    async fn actual_worker_old_automatic_abort_cannot_latch_or_persist_new_run() {
        crate::mode_suite!(Normal, TestFire, Testing);
        let mut state = crate::state::tests::test_app_state().await;
        let (commands, receiver) = tokio::sync::mpsc::channel(4);
        Arc::get_mut(&mut state).unwrap().cmd_tx = commands.clone();
        let old = automatic_generation(&state);
        state
            .automatic_generation
            .store(old + 1, std::sync::atomic::Ordering::Release);
        let dir = std::env::temp_dir().join(format!("old-auto-owner-{}", rand::random::<u128>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("qualification.json");
        state.gse.lock().unwrap().operational.path = Some(path.clone());
        commands
            .try_send(TelemetryCommand::AutomaticAbort { generation: old })
            .unwrap();
        commands
            .try_send(TelemetryCommand::QueuedOperator {
            launch_hold: None,
            recovery_origin: None,
                launch_revision: None,
                generation: old + 1,
                command: Box::new(TelemetryCommand::Postinit),
                plumbing_origin: None,
            })
            .unwrap();
        let (_db_tx, db_rx) = tokio::sync::mpsc::channel(8);
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
            state.clone(),
            router,
            vec![],
            receiver,
            db_rx,
            state.shutdown_subscribe(),
        ));
        let progress = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !state
                .last_command_ms
                .lock()
                .unwrap()
                .contains_key("Postinit")
            {
                tokio::task::yield_now().await;
            }
        })
        .await;
        state.request_shutdown();
        worker.await.unwrap();
        let persisted = std::fs::read(&path).ok();
        let latched = state
            .gse
            .lock()
            .unwrap()
            .operational
            .saved
            .automatic_abort_claimed;
        let phase = state.gse.lock().unwrap().engine.status.phase;
        let _ = std::fs::remove_dir_all(&dir);
        progress.unwrap();
        assert!(
            !latched,
            "stale queued Abort must not mutate a current owner before its epoch check"
        );
        assert_ne!(phase, Phase::Fault);
        assert!(
            persisted.is_none(),
            "stale queued Abort must not write the new run journal"
        );
    }
}

#[cfg(test)]
mod recovery_http_quiescence_tests {
    use super::*;
    use futures::FutureExt;
    use std::sync::atomic::Ordering;
    fn cross_epoch(state: &AppState) {
        state
            .operator_admission_barrier
            .store(77, Ordering::Release);
        state.automatic_generation.fetch_add(1, Ordering::AcqRel);
        state.operator_admission_barrier.store(0, Ordering::Release);
    }
    #[tokio::test]
    async fn real_auth_delayed_clear_cannot_clear_new_run_owner_or_managed_state() {
        crate::mode_suite!(Normal, TestFire, Testing);
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_auth_delayed_clear_cannot_clear_new_run_owner_or_managed_state",
        ) {
            return;
        }
        let (_root, state, _commands, headers) =
            crate::web::command_dispatch_tests::preauth_fixture().await;
        let (state, router, _seen, path) = automatic_abort::tests::fixture_with_state(state).await;
        drop(claim_automatic_abort(&state).unwrap());
        let before = std::fs::read(&path).unwrap();
        let held = state.auth_db.acquire().await.unwrap();
        let request = clear_abort(State(state.clone()), headers.clone());
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        cross_epoch(&state);
        drop(held);
        let stale = request.await.status();
        let latched_after_stale = state
            .gse
            .lock()
            .unwrap()
            .operational
            .saved
            .automatic_abort_claimed;
        let file_after_stale = std::fs::read(&path).unwrap();
        let fresh = clear_abort(State(state.clone()), headers).await.status();
        let fresh_latched = state
            .gse
            .lock()
            .unwrap()
            .operational
            .saved
            .automatic_abort_claimed;
        let cached = router
            .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        let _ = std::fs::remove_file(path);
        assert!(
            latched_after_stale,
            "old handler actually cleared the new run's owner before status check"
        );
        assert_eq!(file_after_stale, before);
        assert_eq!(stale, StatusCode::CONFLICT);
        assert_eq!(fresh, StatusCode::OK);
        assert!(!fresh_latched);
        assert_eq!(cached.unwrap().payload(), &[FlightState::Idle as u8]);
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
    #[tokio::test]
    async fn real_auth_delayed_qualification_reset_cannot_revoke_new_run_confirmation() {
        crate::mode_suite!(Normal, TestFire, Testing);
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_auth_delayed_qualification_reset_cannot_revoke_new_run_confirmation",
        ) {
            return;
        }
        let (root, state, _commands, headers) =
            crate::web::command_dispatch_tests::preauth_fixture().await;
        state.set_local_flight_state(FlightState::Idle);
        let path = root.0.join("qualification.json");
        {
            let mut runtime = state.gse.lock().unwrap();
            runtime.operational.path = Some(path.clone());
            runtime.engine.config.dry_self_test_confirmed = true;
        }
        let held = state.auth_db.acquire().await.unwrap();
        let request = reset_qualification(State(state.clone()), headers.clone());
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        cross_epoch(&state);
        drop(held);
        let stale = request.await.status();
        let confirmation_after_stale = state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed;
        let file_after_stale = std::fs::read(&path).ok();
        let fresh = reset_qualification(State(state.clone()), headers)
            .await
            .status();
        assert!(
            confirmation_after_stale,
            "old reset actually changed current RAM before status assertion"
        );
        assert!(file_after_stale.is_none());
        assert_eq!(stale, StatusCode::CONFLICT);
        assert_eq!(fresh, StatusCode::OK);
        assert!(
            !state
                .gse
                .lock()
                .unwrap()
                .engine
                .config
                .dry_self_test_confirmed
        );
        assert!(path.is_file());
    }
    #[tokio::test]
    async fn real_auth_delayed_confirmation_cannot_grant_new_run_consent() {
        crate::mode_suite!(Normal, TestFire, Testing);
        if crate::operator_queue::isolated_case(
            module_path!(),
            "real_auth_delayed_confirmation_cannot_grant_new_run_consent",
        ) {
            return;
        }
        let (_root, state, _commands, headers) =
            crate::web::command_dispatch_tests::preauth_fixture().await;
        let held = state.auth_db.acquire().await.unwrap();
        let request = self_test_confirmation(
            State(state.clone()),
            headers.clone(),
            Json(SelfTestConfirmation { confirmed: true }),
        );
        tokio::pin!(request);
        assert!(request.as_mut().now_or_never().is_none());
        cross_epoch(&state);
        drop(held);
        let stale = request.await.status();
        let confirmation_after_stale = state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .dry_self_test_confirmed;
        let fresh = self_test_confirmation(
            State(state.clone()),
            headers,
            Json(SelfTestConfirmation { confirmed: true }),
        )
        .await
        .status();
        assert!(
            !confirmation_after_stale,
            "old consent actually reached current runtime"
        );
        assert_eq!(stale, StatusCode::CONFLICT);
        assert_eq!(fresh, StatusCode::OK);
        assert!(
            state
                .gse
                .lock()
                .unwrap()
                .engine
                .config
                .dry_self_test_confirmed
        );
    }
}

#[cfg(test)]
pub(crate) mod nonwaiting_admission_tests;

#[cfg(test)]
pub(crate) fn apply_model_effects_for_test(state: &Arc<AppState>, valves: Vec<(usize, bool)>) {
    apply(
        state,
        Effects {
            valves,
            notification: None,
        },
    );
}

#[cfg(test)]
#[path = "gse/fill_resume_tests.rs"]
mod fill_resume_tests;

#[cfg(test)]
#[path = "gse/pause_resume_tests.rs"]
mod pause_resume_tests;

#[cfg(test)]
pub(crate) async fn fill_recording_test_state(
    mass: f32,
    phase: Phase,
    observer: Option<FillRecordingTestObserver>,
) -> (
    Arc<AppState>,
    Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    Arc<sedsnet::router::Router>,
) {
    let (state, sent, peer) = tests::button_test_state_observed(observer).await;
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    let target = state.fill_targets_snapshot().nitrous;
    {
        let mut rt = state.gse.lock().unwrap();
        rt.engine.nitrous_target_psi = target.target_pressure_psi;
        rt.engine.status.phase = phase;
        rt.engine.status.nitrogen_passed = true;
        rt.engine.status.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.,
            min_psi: 0.,
            max_psi: 0.,
            noise_psi: 0.1,
            samples: 50,
        });
        rt.observe_pressure(Some(0.));
        rt.observe_mass(Some(mass));
        if !crate::runtime_mode::is_hitl() {
            rt.operational.startup_configured = true;
            rt.operational.topology_stable = true;
            let generation = rt.operational.saved.qualification.generation();
            for milestone in [Milestone::SelfTest, Milestone::NitrogenTest] {
                rt.operational
                    .saved
                    .qualification
                    .complete(milestone, generation)
                    .unwrap();
            }
            if phase == Phase::Paused {
                rt.operational
                    .saved
                    .qualification
                    .complete(Milestone::FillTarget, generation)
                    .unwrap();
            }
        }
    }
    (state, sent, peer)
}
#[cfg(test)]
pub(crate) fn pump_fill_recording_test_link(state: &AppState, peer: &sedsnet::router::Router) {
    tests::pump_test_link(state, peer);
}

#[cfg(test)]
pub(crate) fn fill_recording_progress_snapshot(state: &AppState) -> serde_json::Value {
    let rt = state.gse.lock().unwrap();
    let summary = rt
        .progress
        .current()
        .map(|s| (&s.key, s.kind, s.outcome, s.cancel_allowed));
    serde_json::json!({"phase":rt.engine.status.phase,"summary":summary,"marker":format!("{:?}",rt.progress.marker()),"generation":automatic_generation(state)})
}

#[cfg(test)]
type FillRecordingTestObserver = Arc<dyn Fn(&sedsnet::packet::Packet) + Send + Sync>;

/// One live automatic readiness publication, never durable seed mutation.
/// Lock order is DISPATCH.try -> Store.try -> state.try -> policy.try -> model.try -> finite
/// original manual/deferred owners -> board-Abort.try -> SDK Core.try/Schema.try. No provider,
/// filesystem, allocation, callback or packet retirement occurs here.
pub(crate) fn try_commit_automatic_idle(
    state: &AppState,
    original: crate::automatic_startup_idle::Captured,
    identity: crate::network_variables::AutomaticIdleIdentity,
    model: &crate::flight_sim::PreparedAutomaticReadiness,
    prepared: &mut sedsnet::router::PreparedManagedBatch<'_>,
) -> Result<(), crate::automatic_startup_idle::Error> {
    use crate::automatic_startup_idle::Error;
    use std::sync::atomic::Ordering;
    let _dispatch = try_dispatch_gate().ok_or(Error::Busy)?;
    if original.generation != automatic_generation(state)
        || state.operator_admission_barrier.load(Ordering::Acquire) != 0
        || state.shutdown_requested.load(Ordering::Acquire)
        || manual_stop_ram_pending(state) { return Err(Error::Retired); }
    // A retained/admitted Launch intent can precede the visible phase update.
    // Ordinary lamp/Tclock resets cannot re-enable automatic readiness.
    let history = state.launch_history.try_lock().map_err(|_| Error::Busy)?;
    if history.automatic_actions_inhibited() || state.launch_indicator_latched()
        || state.launch_sequence_command_pending.load(Ordering::Acquire) { return Err(Error::Retired); }
    let mut variables = crate::network_variables::try_automatic_idle_store().ok_or(Error::Busy)?;
    if !variables.matches(original.store) { return Err(Error::StoreChanged); }
    let mut current = state.state.try_lock().map_err(|_| Error::Busy)?;
    if *current != FlightState::Startup { return Err(Error::StateChanged); }
    let mut policy = state.sequence_policy_state.try_lock().map_err(|_| Error::Busy)?;
    let mut model = model.try_guard().ok_or(Error::Busy)?;
    let router = state.topology_router.get().ok_or(Error::Retired)?;
    with_stop_admission(state, original.ticket, || {
        state.board_abort.try_admit_automatic_prelaunch(original.ticket.board_abort_revision, || {
        router.try_commit_network_variable_batch_unless_pending_type(prepared, crate::telemetry_schema::types::ABORT)
            .map_err(Error::Sdk)?;
        *current = FlightState::Idle;
        *policy = crate::sequences::SequencePolicyState::default();
        variables.admitted(identity);
        model.admitted();
        Ok(())
        }).map_err(|error| match error {
            crate::board_abort::AutomaticAdmissionError::Busy => Error::Busy,
            _ => Error::Retired,
        })?
    }).map_err(|_| Error::Retired)?
}

/// One original automatic scalar publication. Provider/codec/file work and all
/// prepared-object retirement occur in the caller, outside these try guards.
pub(crate) fn try_commit_automatic_phase(
    state: &AppState,
    proposal: crate::automatic_flight_state::Proposal,
    identity: crate::network_variables::AutomaticPhaseIdentity,
    model: &crate::flight_sim::PreparedAutomaticPhase,
    prepared: &mut sedsnet::router::PreparedManagedBatch<'_>,
) -> Result<(), crate::automatic_flight_state::Error> {
    use crate::automatic_flight_state::{Cause, Error};
    use std::sync::atomic::Ordering;
    let original = proposal.original;
    if !(1..=6).contains(&(proposal.target as u8))
        || identity.phase != proposal.target as u8
        || identity.original.generation != original.generation { return Err(Error::Ineligible); }
    let _dispatch = try_dispatch_gate().ok_or(Error::Busy)?;
    if original.generation != automatic_generation(state)
        || state.operator_admission_barrier.load(Ordering::Acquire) != 0
        || state.shutdown_requested.load(Ordering::Acquire)
        || manual_stop_ram_pending(state) || crate::rrls::is_rrls()
        || crate::runtime_mode::is_hitl() { return Err(Error::Retired); }
    let runtime = state.gse.try_lock().map_err(|_| Error::Busy)?;
    // HITL returned above.
    let eligible = match proposal.cause {
        Cause::Readiness => !crate::runtime_mode::is_test_fire()
            && original.expected == FlightState::Startup && proposal.target == FlightState::Idle,
        Cause::LegacySequence { .. } => !runtime.engine.claimed && !runtime.manual_control,
        Cause::QualifiedArmed { qualification_generation } => {
            !crate::runtime_mode::is_test_fire()
                && proposal.target == FlightState::Armed
                && runtime.operational.saved.qualification.generation() == qualification_generation
                && runtime.operational.saved.qualification.stage() == gse_sequence::qualification::MissionStage::Armed
                && runtime.operational.persist_error.is_none()
                && runtime.operational.saved.pending_checklists.is_empty()
                && matches!(runtime.engine.status.phase, Phase::Paused | Phase::Passed | Phase::Idle)
        }
    };
    if !eligible { return Err(Error::Ineligible); }
    let history = state.launch_history.try_lock().map_err(|_| Error::Busy)?;
    if history.automatic_actions_inhibited() || state.launch_indicator_latched()
        || state.launch_sequence_command_pending.load(Ordering::Acquire) { return Err(Error::Retired); }
    let mut variables = crate::network_variables::try_automatic_idle_store().ok_or(Error::Busy)?;
    if !variables.matches_phase(original.store) { return Err(Error::StoreChanged); }
    let mut current = state.state.try_lock().map_err(|_| Error::Busy)?;
    if *current != original.expected || (*current as u8) > FlightState::Armed as u8 { return Err(Error::StateChanged); }
    let mut policy = state.sequence_policy_state.try_lock().map_err(|_| Error::Busy)?;
    if *policy != original.policy { return Err(Error::PolicyChanged); }
    let mut model = model.try_guard().ok_or(Error::Busy)?;
    let router = state.topology_router.get().ok_or(Error::Retired)?;
    with_stop_admission(state, original.ticket, || {
        state.board_abort.try_admit_automatic_prelaunch(original.ticket.board_abort_revision, || {
        router.try_commit_network_variable_batch_unless_pending_type(prepared, crate::telemetry_schema::types::ABORT)
            .map_err(Error::Sdk)?;
        // All remaining operations are infallible scalar assignments. The
        // physical desired JSON and every clock/session remain untouched.
        *current = proposal.target;
        match proposal.cause {
            Cause::LegacySequence { next_policy } => *policy = next_policy,
            Cause::Readiness => (),
            Cause::QualifiedArmed { .. } => (),
        }
        variables.admitted_phase(identity);
        model.admitted();
        Ok(())
        }).map_err(|error| match error {
            crate::board_abort::AutomaticAdmissionError::Busy => Error::Busy,
            _ => Error::Retired,
        })?
    }).map_err(|_| Error::Retired)?
}

/// Sequence metadata is not a new phase/command. Preserve its original finite
/// Stop/epoch and expected-policy identity as well; never resurrect old policy.
pub(crate) fn try_commit_automatic_policy(
    state: &AppState,
    ticket: LaunchAdmission,
    expected_state: FlightState,
    expected: crate::sequences::SequencePolicyState,
    next: crate::sequences::SequencePolicyState,
) {
    let Some(_dispatch) = try_dispatch_gate() else { return; };
    if state.operator_admission_barrier.load(std::sync::atomic::Ordering::Acquire) != 0
        || manual_stop_state(state) != CurrentState::Current { return; }
    let Ok(history) = state.launch_history.try_lock() else { return; };
    if history.automatic_actions_inhibited() { return; }
    let Ok(current) = state.state.try_lock() else { return; };
    if *current != expected_state { return; }
    let Ok(mut policy) = state.sequence_policy_state.try_lock() else { return; };
    if *policy != expected { return; }
    let _ = with_stop_admission(state, ticket, || {
        state.board_abort.try_admit_automatic_prelaunch(ticket.board_abort_revision, || { *policy = next; })
    });
}

#[cfg(test)]
mod local_stop_priority_tests { include!("gse/local_stop_priority_tests.rs"); }

#[cfg(test)]
pub(crate) fn native_owned_startup_prime(state:&Arc<AppState>) -> serde_json::Value {
    for now in [10000,12000] {
        {let mut rt=state.gse.lock().unwrap();rt.test_now_ms=Some(now);
         rt.engine.config.maximum_zero_offset_psi=Some(5.0);rt.observe_pressure(Some(0.0));}
        tick(state,true);
    }
    native_owned_startup_snapshot(state)
}
#[cfg(test)]
pub(crate) fn native_owned_startup_snapshot(state:&AppState) -> serde_json::Value {
    let rt=state.gse.lock().unwrap();serde_json::json!({"phase":format!("{:?}",rt.engine.status.phase),
        "active":rt.engine.active(),"operational_running":rt.operational.running,
        "startup_configured":rt.operational.startup_configured,"topology_healthy":rt.operational.topology_healthy,
        "topology_stable":rt.operational.topology_stable,"fault":format!("{:?}",rt.operational.saved.qualification.fault())})
}

#[cfg(test)]
mod launch_preparation_contention_tests;

#[cfg(test)]
mod undeliverable_command_tests {
    use super::*;
    use std::sync::Arc;
    fn stalled_router() -> Arc<sedsnet::router::Router> {
        let router = sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("GS"));
        let opts = sedsnet::router::RouterSideOptions { reliable_enabled: true, ..Default::default() };
        let radio = router.add_side_packed_with_options("rocket_comms", |_| Err(sedsnet::TelemetryError::Io("side tx busy")), opts);
        let announce = sedsnet::discovery::build_discovery_announce("FC", 1,
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"), crate::telemetry_schema::endpoint("ABORT")]).unwrap();
        router.rx_from_side(&announce, radio).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        router.tx_queue(sedsnet::packet::Packet::new(crate::telemetry_schema::data_type("ABORT"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")], "GS", 1, Arc::from(&b"SEDS_MANUAL_ABORT_V1:"[..])).unwrap()).unwrap();
        let _ = router.process_all_queues_with_timeout(0);
        Arc::new(router)
    }
    /// The bench fault: an ABORT to the AV bay that the starved radio never
    /// delivers. aed4351 held Clear GSE abort (and the mode switch) on any
    /// pending ABORT, forever. Now both wait while it is still in its normal
    /// delivery window (5 s outstanding), then stop holding; a controller
    /// frame on the umbilical still holds both; HITL clear never waits.
    #[tokio::test]
    async fn an_undeliverable_abort_stops_holding_clear_and_mode_switch_after_its_window() {
        crate::telemetry_schema::initialize().unwrap();
        let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Normal);
        let state = crate::state::tests::test_app_state().await;
        let router = stalled_router();
        state.topology_router.set(router.clone()).unwrap();
        let abort = crate::telemetry_schema::data_type("ABORT");
        assert!(router.has_pending_reliable_tx_type(abort));
        // Still within its delivery window: it holds the clear. Delivery never
        // holds a mode change, which sends no output command.
        assert_eq!(clear_blocked_by_abort_delivery(&state, &router), Ok(true));
        assert_eq!(mode_switch_block_reason(&state), None);
        // Outstanding past the window: neither holds, though the SDK still owns
        // it (the old rule, which refused for as long as it was pending).
        age_delivery_tracking_for_test(UNDELIVERABLE_AFTER + std::time::Duration::from_millis(100));
        assert!(router.has_pending_reliable_tx_type(abort), "the old rule would still refuse");
        assert_eq!(clear_blocked_by_abort_delivery(&state, &router), Ok(false));
        assert_eq!(mode_switch_block_reason(&state), None);
        // A controller frame still being written on the umbilical holds both.
        let bytes = sedsnet::wire_format::pack_packet(&sedsnet::packet::Packet::new(crate::telemetry_schema::data_type("VALVE_COMMAND"),
            &[crate::telemetry_schema::endpoint("VALVE_BOARD")], "GS", 3, Arc::from(&b"\x00"[..])).unwrap()).to_vec();
        let mut held = None;
        state.transport_tx_activity.try_enqueue_owned(true, bytes, |b| { held = Some(b); Ok(()) }).unwrap();
        assert_eq!(clear_blocked_by_abort_delivery(&state, &router), Ok(true));
        assert_eq!(mode_switch_block_reason(&state), None);
        state.transport_tx_activity.complete_frame(&held.unwrap());
        // HITL Clear GSE abort never waits on delivery (unchanged).
        drop(_mode);
        let _hitl = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
        let fresh = stalled_router();
        assert!(!commands_still_in_delivery(&fresh, &[abort]).is_empty(), "a new ABORT is in its window");
        assert_eq!(clear_blocked_by_abort_delivery(&state, &fresh), Ok(false));
        state.request_shutdown();
    }
}
