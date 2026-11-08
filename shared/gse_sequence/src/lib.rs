//! Deterministic ground-fill sequencing. This crate performs no hardware I/O.
use serde::{Deserialize, Serialize};
pub mod cold_flow;
pub mod fill_trim;
pub mod fill_resume;
pub mod qd_wait;
pub mod qualification;
pub mod turbocharge;

/// Order is pilot, vent, dump, nitrogen, nitrous. Ignition/retraction are never tested.
pub const RELIEVED: [bool; 5] = [false, true, true, false, false];
pub const VALVE_ACK_TIMEOUT_MS: u64 = 5000;

pub const CLOSED: [bool; 5] = [false; 5];
pub const FILL_READY: [bool; 5] = [false, true, false, false, false];

pub const MAX_NITROGEN_STEPS: usize = 32;
/// Every non-safety sequence failure notification starts with this text.
pub const FAILURE_PAUSE_PREFIX: &str = "Sequence paused:";
/// A supply that still reports open after its close deadline is a hazard only
/// while tank pressure climbs; a missing pressure reading counts as climbing.
pub const STUCK_SUPPLY_PRESSURE_RISE_PSI: f32 = 5.0;
/// Operator-facing text for a failure that paused (never dumped) the tank.
pub fn failure_pause_notice(reason: &str) -> String {
    format!("{FAILURE_PAUSE_PREFIX} {reason}. Review and Resume or Cancel.")
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NitrogenStep {
    pub target_psi: f32,
    pub settle_ms: u64,
    pub hold_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Independent cold-flow target. Unset disables automatic cold flow.
    pub cold_flow_target_psi: Option<f32>,
    pub nitrogen_target_psi: f32,
    pub nitrogen_steps: Vec<NitrogenStep>,
    pub fill_overfill_kg: f32,
    pub fill_tolerance_kg: f32,
    pub fill_settle_ms: u64,
    /// Explicit additional nitrogen mass allowance, in calibrated kg.
    /// None disables Turbocharge; this is not liquid overfill tolerance.
    pub turbocharge_max_added_mass_kg: Option<f32>,
    /// Closed-tank pressure hysteresis; nitrogen still closes at the exact target.
    pub turbocharge_pressure_deadband_psi: f32,
    pub pressure_step_psi: f32,
    pub pressure_ceiling_psi: Option<f32>,
    pub maximum_zero_offset_psi: Option<f32>,
    pub dry_self_test_confirmed: bool,
    pub grouped_panel: bool,
    /// Tank overpressure relief: at or above this valve-board psi, in every
    /// mode and state before Launch, close both supplies and open dump/vent.
    pub overpressure_relief_psi: f32,
    /// Tank overpressure abort: also open the main (pilot) valve, then Abort.
    pub overpressure_abort_psi: f32,
    /// How long the main (pilot) valve stays open before the Abort.
    pub overpressure_pilot_ms: u64,
    /// Optional physical tank rating; both thresholds must stay below it.
    pub tank_rating_psi: Option<f32>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            cold_flow_target_psi: None,
            nitrogen_target_psi: 500.0,
            nitrogen_steps: Vec::new(),
            fill_overfill_kg: 0.05,
            fill_tolerance_kg: 0.07,
            fill_settle_ms: 3000,
            turbocharge_max_added_mass_kg: None,
            turbocharge_pressure_deadband_psi: 12.0,
            pressure_step_psi: 50.0,
            pressure_ceiling_psi: None,
            maximum_zero_offset_psi: None,
            dry_self_test_confirmed: false,
            grouped_panel: true,
            overpressure_relief_psi: 850.0,
            overpressure_abort_psi: 950.0,
            overpressure_pilot_ms: 3000,
            tank_rating_psi: None,
        }
    }
}
impl Config {
    pub fn validate_overpressure(&self) -> Result<(), String> {
        let relief = self.overpressure_relief_psi;
        let abort = self.overpressure_abort_psi;
        if !relief.is_finite() || !abort.is_finite() || relief <= 0.0 || relief >= abort {
            return Err("Overpressure relief must be finite, positive and below the overpressure abort pressure".into());
        }
        if !(500..=10_000).contains(&self.overpressure_pilot_ms) {
            return Err("Overpressure main-valve opening must be 0.5 to 10 seconds".into());
        }
        match self.tank_rating_psi {
            Some(rating) if !rating.is_finite() || rating <= 0.0 => {
                Err("Tank rating must be finite and positive, or unset".into())
            }
            Some(rating) if abort >= rating => Err(
                "Overpressure relief and abort pressures must both be below the tank rating".into(),
            ),
            _ => Ok(()),
        }
    }
    pub fn validate_turbocharge_deadband(&self) -> Result<(), String> {
        let band = self.turbocharge_pressure_deadband_psi;
        if !band.is_finite() || !(8.0..=16.0).contains(&band) {
            return Err("Turbocharge pressure deadband must be between 8 and 16 psi".into());
        }
        Ok(())
    }
    pub fn nitrogen_target_psi(&self) -> f32 {
        self.nitrogen_steps
            .last()
            .map_or(self.nitrogen_target_psi, |step| step.target_psi)
    }
    pub fn nitrogen_plan(&self) -> Result<Vec<NitrogenStep>, String> {
        let steps = if self.nitrogen_steps.is_empty() {
            if !self.nitrogen_target_psi.is_finite()
                || self.nitrogen_target_psi <= 0.0
                || !self.pressure_step_psi.is_finite()
                || self.pressure_step_psi <= 0.0
            {
                return Err("Nitrogen target and step must be finite and positive".into());
            }
            let count = (self.nitrogen_target_psi / self.pressure_step_psi).ceil();
            if !count.is_finite() || count > MAX_NITROGEN_STEPS as f32 {
                return Err("Nitrogen test supports at most 32 steps".into());
            }
            (1..=count as usize)
                .map(|index| NitrogenStep {
                    target_psi: (index as f32 * self.pressure_step_psi)
                        .min(self.nitrogen_target_psi),
                    settle_ms: 2000,
                    hold_ms: 5000,
                })
                .collect()
        } else {
            self.nitrogen_steps.clone()
        };
        if steps.is_empty() || steps.len() > MAX_NITROGEN_STEPS {
            return Err("Configure 1 to 32 nitrogen steps".into());
        }
        let mut previous = 0.0;
        let mut total = 0u64;
        for step in &steps {
            if !step.target_psi.is_finite()
                || step.target_psi <= previous
                || !(1000..=600000).contains(&step.settle_ms)
                || !(1000..=600000).contains(&step.hold_ms)
            {
                return Err(
                    "Nitrogen targets must ascend; settle and hold waits must be 1 to 600 seconds"
                        .into(),
                );
            }
            previous = step.target_psi;
            total = total
                .saturating_add(step.settle_ms)
                .saturating_add(step.hold_ms);
        }
        if total > 3_600_000 {
            return Err("Nitrogen settle/hold waits must total at most 1 hour".into());
        }
        Ok(steps)
    }
    pub fn validate_fill_trim(&self) -> Result<(), String> {
        if !self.fill_overfill_kg.is_finite()
            || !(0.0..=0.1).contains(&self.fill_overfill_kg)
            || !self.fill_tolerance_kg.is_finite()
            || self.fill_tolerance_kg <= 0.0
            || self.fill_tolerance_kg > 0.07
            || !(1000..=600000).contains(&self.fill_settle_ms)
        {
            return Err("Fill overshoot must be 0 to 0.1 kg, tolerance above 0 and at most 0.07 kg, and settling 1 to 600 seconds".into());
        }
        Ok(())
    }

    pub fn pressure_limit(&self, target: f32, margin: f32) -> Result<f32, String> {
        let limit = target + margin;
        if !target.is_finite() || target <= 0.0 || !limit.is_finite() {
            return Err("Pressure target must be finite and positive".into());
        }
        match self.pressure_ceiling_psi {
            Some(cap) if !cap.is_finite() || cap <= 0.0 => {
                Err("Hardware pressure ceiling must be finite and positive".into())
            }
            Some(cap) => Ok(limit.min(cap)),
            None => Ok(limit),
        }
    }
    /// Incomplete limits may be saved, but cannot enable automatic operations.
    pub fn validate_settings(&self) -> Result<(), String> {
        if self.cold_flow_target_psi.is_some_and(|target| !target.is_finite() || target <= 0.0
            || self.pressure_ceiling_psi.is_some_and(|ceiling| !ceiling.is_finite() || target >= ceiling)) {
            return Err("Cold-flow target must be finite, positive and below the configured hardware ceiling, or unset".into());
        }
        self.nitrogen_plan()?;
        self.validate_fill_trim()?;
        self.validate_turbocharge_deadband()?;
        self.validate_overpressure()?;
        if self
            .turbocharge_max_added_mass_kg
            .is_some_and(|v| !v.is_finite() || v <= 0.0)
        {
            return Err(
                "Turbocharge maximum added nitrogen mass must be finite and positive, or unset"
                    .into(),
            );
        }
        if !self.nitrogen_target_psi.is_finite()
            || !self.pressure_step_psi.is_finite()
            || self.pressure_step_psi <= 0.0
            || self.nitrogen_target_psi <= 0.0
            || self
                .pressure_ceiling_psi
                .is_some_and(|v| !v.is_finite() || v <= 0.0)
            || self
                .maximum_zero_offset_psi
                .is_some_and(|v| !v.is_finite() || v < 0.0)
        {
            return Err("Pressure settings must be finite and nonnegative; target and ceiling must be positive".into());
        }
        if self.maximum_zero_offset_psi.is_some() {
            self.validate()?;
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        self.nitrogen_plan()?;
        self.validate_fill_trim()?;
        let target = self.nitrogen_target_psi();
        let ceiling = self.pressure_limit(target, 100.0)?;
        let zero = self
            .maximum_zero_offset_psi
            .ok_or("Set the maximum acceptable empty-tank PT offset")?;
        if !ceiling.is_finite()
            || !self.pressure_step_psi.is_finite()
            || self.pressure_step_psi <= 0.0
            || !zero.is_finite()
            || !self.nitrogen_target_psi.is_finite()
            || zero < 0.0
            || ceiling <= zero
            || self.nitrogen_target_psi <= 0.0
            || target + zero >= ceiling
        {
            return Err("Target plus zero-offset allowance must be below the pressure ceiling; all limits must be finite and positive".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub value: f32,
    pub at_ms: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct ValveState {
    pub open: bool,
    pub at_ms: u64,
}
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    pub now_ms: u64,
    pub pressure: Option<Sample>,
    pub valves: [Option<ValveState>; 5],
    pub interlock: bool,
    pub prelaunch: bool,
}

/// Copy-only reporting values. Targets are plans; these do not prove physical valve motion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ValveProgressObservation {
    pub target_open: bool,
    pub raw_readback_open: Option<bool>,
    pub fresh_readback_open: Option<bool>,
    pub readback_age_ms: Option<u64>,
    pub causal_since_ms: u64,
    pub awaiting_dispatch: bool,
    pub awaiting_self_readback: bool,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PressureStepProgressObservation {
    pub target_psi: f32,
    pub settle_ms: u64,
    pub hold_ms: u64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressObservation {
    pub phase: Phase,
    pub phase_since_ms: u64,
    pub phase_elapsed_ms: Option<u64>,
    pub command_since_ms: u64,
    pub self_index: usize,
    pub self_pending_mask: u8,
    pub self_waiting: Option<usize>,
    pub nitrogen_step_index: usize,
    pub nitrogen_step_count: usize,
    pub nitrogen_step: Option<PressureStepProgressObservation>,
    pub valves: [ValveProgressObservation; 5],
    pub fresh_pressure_psi: Option<f32>,
    pub nitrogen_absolute_target_psi: Option<f32>,
    pub pressure_age_ms: Option<u64>,
    pub all_valves_confirmed_by_engine: bool,
    pub hold_samples: usize,
    pub hold_reference_psi: Option<f32>,
    /// The nitrogen test's final hold passed and now holds pressure.
    pub test_hold_passed: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    StartFill,
    PauseFill,
    CancelFill,
    SelfTest,
    NitrogenTest,
    Turbocharge,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    BaselineSetup,
    Baseline,
    SelfSetup,
    SelfOpen,
    SelfHold,
    SelfClose,
    SelfFinish,
    NitrogenSetup,
    Raising,
    Settling,
    Holding,
    Dumping,
    Passed,
    FillSetup,
    Filling,
    PauseSetup,
    Paused,
    CancelSetup,
    Cancelled,
    Fault,
    Turbocharge,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Baseline {
    pub average_psi: f32,
    pub min_psi: f32,
    pub max_psi: f32,
    pub noise_psi: f32,
    pub samples: usize,
}
impl Baseline {
    pub fn is_zero(&self, value: f32) -> bool {
        value.is_finite() && (value - self.average_psi).abs() <= self.noise_psi.max(0.01)
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub phase: Phase,
    pub message: String,
    pub nitrogen_passed: bool,
    pub nitrogen_overridden: bool,
    pub self_test_locked: bool,
    pub current_step_psi: f32,
    pub baseline: Option<Baseline>,
    pub pressure_psi: Option<f32>,
    pub valves: [Option<bool>; 5],
}
#[derive(Default, Debug)]
pub struct Effects {
    pub valves: Vec<(usize, bool)>,
    pub notification: Option<String>,
}

pub struct Engine {
    pub config: Config,
    /// The operator, not an automatic fill sequence, controls nitrous flow
    /// (e.g. automatic cutoff disabled). A nitrous overfill then warns and
    /// never dumps; tank overpressure relief is enforced separately.
    pub manual_fill_control: bool,
    overfill_warned: bool,
    /// Supplied from the nitrous fill target before requesting StartFill.
    pub nitrous_target_psi: f32,
    nitrous_session: bool,
    // Captured before a new StartFill changes nitrous_session or phase.
    resume_existing_inventory: bool,
    pub status: Status,
    pub claimed: bool,
    phase_since: u64,
    command_since: u64,
    target: [bool; 5],
    samples: Vec<Sample>,
    self_index: usize,
    testing_valves: bool,
    self_ack_since: [u64; 5],
    self_pending: u8,
    self_waiting: Option<usize>,
    nitrogen_plan: Vec<NitrogenStep>,
    nitrogen_step_index: usize,
    hold_reference: Option<f32>,
    hold_samples: usize,
    /// Test phase interrupted by an operator pause. Only a paused valve
    /// self-test or nitrogen test carries this; fill pauses never do.
    paused_from: Option<Phase>,
    /// Reason a non-safety failure paused this sequence. Cleared only by an
    /// explicit operator request (pause, resume, cancel, start) or a fault.
    failure: Option<String>,
    /// Fresh tank pressure when the current pause began closing.
    pause_reference_psi: Option<f32>,
    /// Nitrogen test result held at pressure for the operator's choice.
    test_hold: Option<TestHold>,
    /// The operator chose Dump: wait (without a deadline) for ~0 psi.
    dump_to_zero: bool,
}
/// The nitrogen test ended at pressure and holds it closed for the operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum TestHold {
    /// Every hold step passed; the tank holds the top-step pressure.
    Passed,
    /// A leak verdict; pressure is held for a leak search.
    Failed(String),
}
impl TestHold {
    pub fn message(&self) -> String {
        match self {
            Self::Passed => "Nitrogen test passed: holding top-step pressure. Choose Cold flow or Dump.".into(),
            Self::Failed(reason) => format!(
                "Nitrogen test failed: {reason}. Holding pressure for leak search. Dump when ready."
            ),
        }
    }
}
/// Which qualification test an operator pause interrupted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausedTest {
    ValveSelfTest,
    NitrogenTest,
}
fn test_phase(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::BaselineSetup
            | Phase::Baseline
            | Phase::SelfSetup
            | Phase::SelfOpen
            | Phase::SelfHold
            | Phase::SelfClose
            | Phase::SelfFinish
            | Phase::NitrogenSetup
            | Phase::Raising
            | Phase::Settling
            | Phase::Holding
            | Phase::Dumping
    )
}
impl Default for Engine {
    fn default() -> Self {
        Self {
            config: Config::default(),
            manual_fill_control: false,
            overfill_warned: false,
            nitrous_target_psi: 750.0,
            nitrous_session: false,
            resume_existing_inventory: false,
            status: Status {
                phase: Phase::Idle,
                message: "Ready for GSE checkout".into(),
                nitrogen_passed: false,
                nitrogen_overridden: false,
                self_test_locked: false,
                current_step_psi: 0.0,
                baseline: None,
                pressure_psi: None,
                valves: [None; 5],
            },
            claimed: false,
            phase_since: 0,
            command_since: 0,
            target: CLOSED,
            samples: Vec::new(),
            self_index: 0,
            testing_valves: false,
            self_ack_since: [0; 5],
            self_pending: 0,
            self_waiting: None,
            nitrogen_plan: Vec::new(),
            nitrogen_step_index: 0,
            hold_reference: None,
            hold_samples: 0,
            paused_from: None,
            failure: None,
            pause_reference_psi: None,
            test_hold: None,
            dump_to_zero: false,
        }
    }
}
impl Engine {
    /// Establish startup outputs only after the caller verifies live routes,
    /// fresh pressure and the operating interlock. A qualified filled tank is
    /// held closed; an empty checkout opens dump and vent.
    pub fn prepare_startup(&mut self, now_ms: u64, hold_filled: bool) -> Effects {
        self.resume_existing_inventory = false;
        self.paused_from = None;
        self.failure = None;
        self.test_hold = None;
        self.dump_to_zero = false;
        self.claimed = true;
        self.testing_valves = false;
        self.nitrous_session = hold_filled;
        self.configure(
            if hold_filled {
                Phase::PauseSetup
            } else {
                Phase::CancelSetup
            },
            if hold_filled { CLOSED } else { RELIEVED },
            now_ms,
            "Confirming startup valve configuration",
        )
    }
    pub fn active(&self) -> bool {
        !matches!(
            self.status.phase,
            Phase::Idle | Phase::Passed | Phase::Cancelled | Phase::Fault
        )
    }
    fn fresh_pressure(input: Inputs) -> Option<f32> {
        input
            .pressure
            .filter(|p| {
                p.value.is_finite() && p.at_ms <= input.now_ms && input.now_ms - p.at_ms <= 2000
            })
            .map(|p| p.value)
    }
    fn fill_entry_pressure_ready(&self, pressure: f32, continuing_inventory: bool) -> bool {
        self.status.baseline.as_ref().is_some_and(|baseline| {
            if continuing_inventory {
                pressure.is_finite()
                    && pressure >= 0.0
                    && self.config.pressure_limit(self.nitrous_target_psi, 50.0)
                        .is_ok_and(|limit| limit > self.nitrous_target_psi && pressure < limit)
            } else {
                baseline.is_zero(pressure)
            }
        })
    }
    pub fn allows(&self, action: Action, input: Inputs) -> bool {
        if action == Action::CancelFill {
            return input.prelaunch;
        }
        if !input.prelaunch || !input.interlock {
            return false;
        }
        if action == Action::PauseFill {
            return matches!(
                self.status.phase,
                Phase::FillSetup | Phase::Filling | Phase::Paused
            ) || test_phase(self.status.phase);
        }
        if self.config.validate().is_err() || Self::fresh_pressure(input).is_none() {
            return false;
        }
        match action {
            Action::SelfTest => {
                !self.active()
                    && !self.status.self_test_locked
                    && self.config.dry_self_test_confirmed
            }
            Action::NitrogenTest => !self.active(),
            Action::StartFill => {
                self.config
                    .pressure_limit(self.nitrous_target_psi, 50.0)
                    .is_ok_and(|limit| limit > self.nitrous_target_psi)
                    && (self.status.nitrogen_passed || self.status.nitrogen_overridden)
                    && self.paused_from.is_none()
                    && matches!(self.status.phase, Phase::Passed | Phase::Paused)
                    && self.fill_entry_pressure_ready(
                        Self::fresh_pressure(input).unwrap(),
                        self.nitrous_session && self.status.phase == Phase::Paused,
                    )
            }
            _ => false,
        }
    }
    fn transition(&mut self, phase: Phase, now: u64, message: &str) {
        if phase != Phase::FillSetup {
            self.resume_existing_inventory = false;
        }
        self.status.phase = phase;
        self.phase_since = now;
        self.status.message = message.into();
    }
    fn configure(&mut self, phase: Phase, desired: [bool; 5], now: u64, message: &str) -> Effects {
        if phase == Phase::PauseSetup && self.status.phase != Phase::PauseSetup {
            self.pause_reference_psi = self.status.pressure_psi;
        }
        self.self_waiting = None;
        self.command_since = now;
        self.target = desired;
        self.transition(phase, now, message);
        if phase == Phase::Fault {
            self.self_pending = 0;
            return Effects {
                valves: [3, 4, 0, 1, 2]
                    .into_iter()
                    .map(|i| (i, desired[i]))
                    .collect(),
                notification: None,
            };
        }
        // Each new target gets fresh board valve confirmation, independent of the
        // cached command. Only one operation is outstanding at a time.
        self.self_pending = 0b11111;
        self.self_ack_since = [now; 5];
        self.next_self_command(now)
    }
    fn self_confirmation_phase(&self) -> bool {
        self.status.phase != Phase::Fault
    }
    fn configure_self(
        &mut self,
        phase: Phase,
        desired: [bool; 5],
        input: Inputs,
        message: &str,
    ) -> Effects {
        self.self_pending = 0;
        self.self_waiting = None;
        for (i, &target) in desired.iter().enumerate() {
            let already_confirmed = input.valves[i].is_some_and(|v| {
                v.open == target
                    && v.at_ms <= input.now_ms
                    && if phase == Phase::BaselineSetup {
                        input.now_ms - v.at_ms <= 2000
                    } else {
                        self.target[i] == target && v.at_ms >= self.self_ack_since[i]
                    }
            });
            if already_confirmed {
                self.self_ack_since[i] = input.valves[i].unwrap().at_ms;
            } else {
                self.self_pending |= 1 << i;
                self.self_ack_since[i] = input.now_ms;
            }
        }
        self.target = desired;
        self.transition(phase, input.now_ms, message);
        self.next_self_command(input.now_ms)
    }
    fn next_self_command(&mut self, now: u64) -> Effects {
        // Establish safe supply states first; never burst both vent/dump moves.
        let order = [3, 4, 0, 1, 2];
        let next = order
            .into_iter()
            .find(|i| self.self_pending & (1 << i) != 0 && !self.target[*i])
            .or_else(|| {
                order
                    .into_iter()
                    .find(|i| self.self_pending & (1 << i) != 0)
            });
        self.self_waiting = next;
        if let Some(i) = next {
            self.self_pending &= !(1 << i);
            self.self_ack_since[i] = now;
            Effects {
                valves: vec![(i, self.target[i])],
                notification: None,
            }
        } else {
            Effects::default()
        }
    }
    fn validate_request(&self, action: Action, input: Inputs) -> Result<(), String> {
        if !self.allows(action, input) {
            if !input.prelaunch {
                return Err("GSE action blocked: vehicle is not in a prelaunch state".into());
            }
            if !input.interlock {
                return Err("GSE action blocked: enable the key/software interlock".into());
            }
            if !matches!(action, Action::PauseFill | Action::CancelFill) {
                self.config.validate()?;
                if action == Action::StartFill {
                    let limit = self.config.pressure_limit(self.nitrous_target_psi, 50.0)?;
                    if limit <= self.nitrous_target_psi {
                        return Err(
                            "Nitrous target must be below the hardware pressure ceiling".into()
                        );
                    }
                }
                if Self::fresh_pressure(input).is_none() {
                    return Err("GSE action blocked: a fresh tank PT sample is required".into());
                }
            }
            return Err(
                "GSE action blocked: check limits, fresh PT, sequence state and interlocks".into(),
            );
        }
        Ok(())
    }

    /// Side-effect-free admission preview, including all existing engine safety gates.
    pub fn fill_resume_required(
        &self,
        input: Inputs,
        mass: Option<Sample>,
        target_mass_kg: f32,
    ) -> Result<bool, String> {
        self.validate_request(Action::StartFill, input)?;
        fill_resume::resume_required(input.now_ms, mass, target_mass_kg)
    }

    /// Explicit fill admission with the selected calibrated kg sample and configured target.
    /// A no-op never claims the engine, changes phase, or produces valve effects.
    pub fn request_fill(
        &mut self,
        input: Inputs,
        mass: Option<Sample>,
        target_mass_kg: f32,
    ) -> Result<fill_resume::FillStart, String> {
        if !self.fill_resume_required(input, mass, target_mass_kg)? {
            return Ok(fill_resume::FillStart::Noop);
        }
        self.request(Action::StartFill, input)
            .map(fill_resume::FillStart::Started)
    }

    pub fn request(&mut self, action: Action, input: Inputs) -> Result<Effects, String> {
        self.validate_request(action, input)?;
        // An explicit operator request acknowledges an earlier failure pause.
        self.failure = None;
        self.resume_existing_inventory = action == Action::StartFill
            && self.status.phase == Phase::Paused
            && self.nitrous_session;
        if action == Action::PauseFill {
            // Re-pausing an already paused test keeps its resume point.
            if test_phase(self.status.phase) {
                self.paused_from = Some(self.status.phase);
            } else if self.status.phase != Phase::Paused {
                self.paused_from = None;
            }
        } else {
            self.paused_from = None;
        }
        self.claimed = true;
        Ok(match action {
            Action::Turbocharge => {
                return Err("Turbocharge requires its qualified inventory controller".into());
            }
            // From a nitrogen-test pressure hold, Cancel is the Dump choice:
            // dump and vent, never the main valve, then end at ~0 psi.
            Action::CancelFill if self.test_hold.is_some() || self.dump_to_zero => {
                self.dump_to_zero = true;
                self.configure(
                    Phase::Dumping,
                    RELIEVED,
                    input.now_ms,
                    "Dumping: dump and vent open; ends when the tank reaches its zero-pressure band",
                )
            }
            Action::CancelFill => {
                self.status.nitrogen_passed = false;
                self.status.nitrogen_overridden = false;
                self.configure(
                    Phase::CancelSetup,
                    RELIEVED,
                    input.now_ms,
                    "Cancelling: closing supplies and opening dump/vent",
                )
            }
            Action::PauseFill => self.configure(
                Phase::PauseSetup,
                CLOSED,
                input.now_ms,
                "Pausing: closing all valves",
            ),
            Action::StartFill => {
                self.nitrous_session = true;
                self.status.self_test_locked = true;
                self.configure(
                    Phase::FillSetup,
                    FILL_READY,
                    input.now_ms,
                    "Configuring for nitrous fill",
                )
            }
            Action::SelfTest | Action::NitrogenTest => {
                self.nitrous_session = false;
                self.nitrogen_plan = self.config.nitrogen_plan()?;
                self.nitrogen_step_index = 0;
                self.status.nitrogen_passed = false;
                self.status.nitrogen_overridden = false;
                self.status.baseline = None;
                self.testing_valves = action == Action::SelfTest;
                self.config.dry_self_test_confirmed = false;
                if !self.testing_valves {
                    self.status.self_test_locked = true;
                }
                self.samples.clear();
                if self.testing_valves {
                    return Ok(self.configure_self(
                        Phase::BaselineSetup,
                        RELIEVED,
                        input,
                        "Preparing empty-tank pressure calibration",
                    ));
                }
                self.configure(
                    Phase::BaselineSetup,
                    RELIEVED,
                    input.now_ms,
                    "Preparing empty-tank pressure calibration",
                )
            }
        })
    }
    /// A failed qualification is recoverable: stop filling, verify the relief
    /// configuration, and require a new test without globally aborting the vehicle.
    pub fn cancel_failed_test(&mut self, now: u64, message: &str) -> Effects {
        self.paused_from = None;
        self.status.nitrogen_passed = false;
        self.status.nitrogen_overridden = false;
        let mut effects = self.configure(Phase::CancelSetup, RELIEVED, now, message);
        effects.notification = Some(format!(
            "GSE test cancelled: {message}. Verify pressure and correct the cause before repeating the test."
        ));
        effects
    }
    /// The nitrogen test result held at pressure, if any.
    pub fn test_hold(&self) -> Option<&TestHold> {
        self.test_hold.as_ref()
    }
    /// The operator's Dump choice is venting the held test pressure.
    pub fn dumping_to_zero(&self) -> bool {
        self.dump_to_zero && self.status.phase == Phase::Dumping
    }
    /// End the nitrogen test at pressure: close every valve and hold. A leak
    /// verdict holds for a leak search; a pass offers Cold flow or Dump.
    fn hold_test_pressure(&mut self, now: u64, hold: TestHold) -> Effects {
        let message = hold.message();
        self.paused_from = None;
        self.hold_reference = None;
        self.hold_samples = 0;
        self.status.nitrogen_passed = hold == TestHold::Passed;
        self.status.nitrogen_overridden = false;
        self.failure = match &hold {
            TestHold::Failed(reason) => Some(format!("Nitrogen test failed: {reason}")),
            TestHold::Passed => None,
        };
        self.test_hold = Some(hold);
        let mut effects = self.configure(Phase::PauseSetup, CLOSED, now, &message);
        effects.notification = Some(message);
        effects
    }
    /// Reason the sequence is held in a failure pause, if any.
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
    /// The supply/pressure ceiling for the current session.
    pub fn session_pressure_limit(&self) -> Result<f32, String> {
        if self.nitrous_session {
            self.config.pressure_limit(self.nitrous_target_psi, 50.0)
        } else {
            self.config
                .pressure_limit(self.config.nitrogen_target_psi(), 100.0)
        }
    }
    /// A non-safety failure never opens vent or dump. Close both supplies
    /// first, then every valve, and hold Paused (fresh board confirmation is
    /// still required) until the operator resumes, cancels or takes manual
    /// control. An operator cancel or an existing fault keeps its relief.
    pub fn pause_for_failure(&mut self, now: u64, message: &str) -> Effects {
        let from = self.status.phase;
        if matches!(
            from,
            Phase::Idle | Phase::Passed | Phase::Cancelled | Phase::CancelSetup | Phase::Fault
        ) {
            return Effects::default();
        }
        let repeated = from == Phase::PauseSetup && self.failure.as_deref() == Some(message);
        if test_phase(from) || from == Phase::SelfFinish {
            // Infrastructure failures resume from the interrupted test step.
            self.paused_from = Some(from);
        } else if !matches!(from, Phase::PauseSetup | Phase::Paused) {
            self.paused_from = None;
        }
        self.claimed = true;
        self.failure = Some(message.into());
        self.hold_reference = None;
        self.hold_samples = 0;
        let mut effects = self.configure(
            Phase::PauseSetup,
            CLOSED,
            now,
            &format!("{FAILURE_PAUSE_PREFIX} {message}; closing all valves"),
        );
        // Both supplies close at once; the sequenced confirmation still
        // re-commands and confirms each valve individually.
        if effects.valves == [(3, false)] {
            effects.valves.push((4, false));
        }
        if !repeated {
            effects.notification = Some(failure_pause_notice(message));
        }
        effects
    }
    /// Already-closed pause: record the reason once and command nothing.
    fn hold_paused_for_failure(&mut self, message: &str) -> Effects {
        if self.failure.as_deref() == Some(message) {
            return Effects::default();
        }
        self.failure = Some(message.into());
        self.status.message = format!("{FAILURE_PAUSE_PREFIX} {message}; holding all valves closed");
        Effects {
            valves: Vec::new(),
            notification: Some(failure_pause_notice(message)),
        }
    }
    fn pause_or_hold_for_failure(&mut self, input: Inputs, message: &str) -> Effects {
        if self.status.phase != Phase::Paused {
            return self.pause_for_failure(input.now_ms, message);
        }
        // Valve supervision continues while the pause holds.
        if self.valve_ack_expired(input) {
            return self.valve_ack_timeout(input);
        }
        self.hold_paused_for_failure(message)
    }
    /// Session ceiling: an automatic sequence relieves; a nitrous overfill
    /// under manual fill control only warns (once per excursion).
    fn pressure_ceiling_reached(&mut self, now: u64, pressure: f32) -> Effects {
        if !(self.nitrous_session && self.manual_fill_control) {
            return self.fault(now, "Pressure ceiling reached");
        }
        if self.overfill_warned {
            return Effects::default();
        }
        self.overfill_warned = true;
        let message = format!(
            "Nitrous overfill warning: tank pressure {pressure:.0} psi is at the fill limit under manual control; no automatic dump"
        );
        self.status.message = message.clone();
        Effects {
            valves: Vec::new(),
            notification: Some(message),
        }
    }
    pub fn fault(&mut self, now: u64, message: &str) -> Effects {
        self.failure = None;
        self.test_hold = None;
        self.dump_to_zero = false;
        self.paused_from = None;
        self.status.nitrogen_passed = false;
        self.status.nitrogen_overridden = false;
        let mut effects = self.configure(Phase::Fault, RELIEVED, now, message);
        effects.notification = Some(format!(
            "GSE sequence failed: {message}. Supply close and dump/vent open requested; verify tank pressure."
        ));
        effects
    }
    pub fn manual_override(&mut self) {
        self.resume_existing_inventory = false;
        self.paused_from = None;
        self.failure = None;
        self.test_hold = None;
        self.dump_to_zero = false;
        self.claimed = false;
        self.testing_valves = false;
        self.self_pending = 0;
        self.self_waiting = None;
        self.hold_reference = None;
        self.hold_samples = 0;
        self.samples.clear();
        if self.status.phase != Phase::Fault {
            self.status.phase = Phase::Idle;
            self.status.message = "Automation cancelled; manual valve control".into();
        }
    }
    pub fn nitrous_session(&self) -> bool {
        self.nitrous_session
    }
    /// The test an operator pause is closing or holding, if any.
    pub fn paused_test(&self) -> Option<PausedTest> {
        (self.paused_from.is_some()
            && matches!(self.status.phase, Phase::PauseSetup | Phase::Paused))
        .then_some(if self.testing_valves {
            PausedTest::ValveSelfTest
        } else {
            PausedTest::NitrogenTest
        })
    }
    /// Side-effect-free preview of every gate `resume_test` applies.
    pub fn resume_test_check(&self, input: Inputs) -> Result<(), String> {
        if !input.prelaunch {
            return Err("GSE action blocked: vehicle is not in a prelaunch state".into());
        }
        if !input.interlock {
            return Err("GSE action blocked: enable the key/software interlock".into());
        }
        if self.status.phase != Phase::Paused || self.paused_from.is_none() {
            return Err("Only a paused valve self-test or nitrogen test can resume".into());
        }
        self.config.validate()?;
        let pressure = Self::fresh_pressure(input)
            .ok_or("GSE action blocked: a fresh tank PT sample is required")?;
        let limit = self
            .config
            .pressure_limit(self.config.nitrogen_target_psi(), 100.0)?;
        if pressure >= limit {
            return Err("Tank pressure is at the test pressure ceiling".into());
        }
        if !self.confirmed(input) {
            return Err(
                "All five valves must freshly report closed after the pause before resuming"
                    .into(),
            );
        }
        Ok(())
    }
    /// Resume a paused test from a safe re-entry point:
    /// - pause during the empty-tank baseline repeats the baseline;
    /// - valve self-test re-checks resting outputs, then re-tests the valve it
    ///   was on (or finishes if every valve was already tested);
    /// - nitrogen test resumes the current pressure step with fresh settle and
    ///   hold timers; if the tank is still at or above that step it goes
    ///   straight to settling, so nitrogen is not reopened. A pause while
    ///   dumping resumes dumping.
    pub fn resume_test(&mut self, input: Inputs) -> Result<Effects, String> {
        self.resume_test_check(input)?;
        let from = self.paused_from.take().expect("checked paused test");
        self.failure = None;
        self.claimed = true;
        self.hold_reference = None;
        self.hold_samples = 0;
        if matches!(from, Phase::BaselineSetup | Phase::Baseline) || self.status.baseline.is_none()
        {
            self.samples.clear();
            self.status.baseline = None;
            let message = "Resuming: repeating the empty-tank pressure calibration";
            return Ok(if self.testing_valves {
                self.configure_self(Phase::BaselineSetup, RELIEVED, input, message)
            } else {
                self.configure(Phase::BaselineSetup, RELIEVED, input.now_ms, message)
            });
        }
        if self.testing_valves {
            return Ok(if self.self_index >= 5 || from == Phase::SelfFinish {
                self.configure_self(
                    Phase::SelfFinish,
                    RELIEVED,
                    input,
                    "Resuming: restoring dump and vent to open",
                )
            } else {
                self.configure_self(
                    Phase::SelfSetup,
                    RELIEVED,
                    input,
                    "Resuming: checking resting valve states, then re-testing the current valve",
                )
            });
        }
        if from == Phase::Dumping {
            return Ok(self.configure(
                Phase::Dumping,
                RELIEVED,
                input.now_ms,
                "Resuming: dumping tank before enabling fill",
            ));
        }
        let pressure = Self::fresh_pressure(input).expect("checked fresh pressure");
        let step = self
            .status
            .baseline
            .as_ref()
            .map(|b| b.average_psi + self.status.current_step_psi);
        Ok(if step.is_some_and(|step| pressure >= step) {
            self.configure(
                Phase::Settling,
                CLOSED,
                input.now_ms,
                "Resuming: tank already at the current step; settling before a fresh hold",
            )
        } else {
            self.configure(
                Phase::NitrogenSetup,
                CLOSED,
                input.now_ms,
                "Resuming the current nitrogen pressure step",
            )
        })
    }
    fn nitrogen_step(&self) -> NitrogenStep {
        self.nitrogen_plan
            .get(self.nitrogen_step_index)
            .cloned()
            .unwrap_or(NitrogenStep {
                target_psi: self.status.current_step_psi,
                settle_ms: 2000,
                hold_ms: 5000,
            })
    }
    /// Reads existing engine evidence without advancing its clocks, predicates or outputs.
    pub fn progress_observation(&self, input: Inputs) -> ProgressObservation {
        let self_confirmation=self.self_confirmation_phase();
        ProgressObservation {
            phase:self.status.phase,
            phase_since_ms:self.phase_since,
            phase_elapsed_ms:input.now_ms.checked_sub(self.phase_since),
            command_since_ms:self.command_since,
            self_index:self.self_index,
            self_pending_mask:self.self_pending,
            self_waiting:self.self_waiting,
            nitrogen_step_index:self.nitrogen_step_index,
            nitrogen_step_count:self.nitrogen_plan.len(),
            nitrogen_step:self.nitrogen_plan.get(self.nitrogen_step_index).map(|step|PressureStepProgressObservation{
                target_psi:step.target_psi,settle_ms:step.settle_ms,hold_ms:step.hold_ms,
            }),
            valves:std::array::from_fn(|i| {
                let readback=input.valves[i];
                let age=readback.and_then(|v|input.now_ms.checked_sub(v.at_ms));
                ValveProgressObservation {
                    target_open:self.target[i],raw_readback_open:readback.map(|v|v.open),
                    fresh_readback_open:readback.filter(|_|age.is_some_and(|age|age<=2000)).map(|v|v.open),
                    readback_age_ms:age,
                    causal_since_ms:if self_confirmation {self.self_ack_since[i]}else{self.command_since},
                    awaiting_dispatch:self_confirmation && self.self_pending&(1<<i)!=0,
                    awaiting_self_readback:self_confirmation && self.self_waiting==Some(i),
                }
            }),
            fresh_pressure_psi:Self::fresh_pressure(input),
            nitrogen_absolute_target_psi:if matches!(self.status.phase,Phase::Raising|Phase::Settling|Phase::Holding) {
                self.status.baseline.as_ref().map(|b|b.average_psi+self.status.current_step_psi).filter(|v|v.is_finite())
            }else{None},
            pressure_age_ms:input.pressure.and_then(|v|input.now_ms.checked_sub(v.at_ms)),
            all_valves_confirmed_by_engine:self.confirmed(input),
            hold_samples:self.hold_samples,
            hold_reference_psi:self.hold_reference.filter(|value|value.is_finite()),
            test_hold_passed:self.test_hold==Some(TestHold::Passed),
        }
    }
    /// The actual already validated plan, bounded by MAX_NITROGEN_STEPS.
    pub fn progress_nitrogen_plan(&self)->&[NitrogenStep] {&self.nitrogen_plan}

    fn confirmed(&self, input: Inputs) -> bool {
        input
            .valves
            .iter()
            .zip(self.target)
            .enumerate()
            .all(|(i, (v, target))| {
                v.is_some_and(|v| {
                    v.open == target
                        && v.at_ms <= input.now_ms
                        && input.now_ms - v.at_ms <= 2000
                        && v.at_ms
                            >= if self.self_confirmation_phase() {
                                self.self_ack_since[i]
                            } else {
                                self.command_since
                            }
                })
            })
            && (!self.self_confirmation_phase()
                || (self.self_pending == 0 && self.self_waiting.is_none()))
    }
    fn valve_ack_expired(&self, input: Inputs) -> bool {
        input
            .valves
            .iter()
            .zip(self.target)
            .enumerate()
            .any(|(i, (v, target))| {
                // A queued self-test valve gets its own five seconds only after dispatch.
                if self.self_confirmation_phase() && self.self_pending & (1 << i) != 0 {
                    return false;
                }
                let since = if self.self_confirmation_phase() {
                    self.self_ack_since[i]
                } else {
                    self.command_since
                };
                !v.is_some_and(|v| {
                    v.open == target
                        && if self.self_waiting == Some(i) {
                            v.at_ms > since
                        } else {
                            v.at_ms >= since
                        }
                        && v.at_ms <= input.now_ms
                        && input.now_ms - v.at_ms <= 2000
                }) && input.now_ms.saturating_sub(since) >= VALVE_ACK_TIMEOUT_MS
            })
    }
    fn valve_ack_timeout(&mut self, input: Inputs) -> Effects {
        let names = ["Pilot", "Vent", "Dump", "Nitrogen", "Nitrous"];
        let pending: Vec<String> = input
            .valves
            .iter()
            .zip(self.target)
            .enumerate()
            .filter_map(|(i, (observed, target))| {
                let wanted = if target { "open" } else { "closed" };
                let reason = match observed {
                    None => "no report",
                    Some(v) if v.at_ms > input.now_ms || input.now_ms - v.at_ms > 2000 => {
                        "no fresh report"
                    }
                    Some(v) if self.self_waiting == Some(i) && v.at_ms == self.self_ack_since[i] => {
                        "no post-command report"
                    }
                    Some(v)
                        if v.at_ms
                            < if self.self_confirmation_phase() {
                                self.self_ack_since[i]
                            } else {
                                self.command_since
                            } =>
                    {
                        "no fresh report"
                    }
                    Some(v) if v.open != target => "opposite state reported",
                    _ => return None,
                };
                Some(format!("{} {wanted}: {reason}", names[i]))
            })
            .collect();
        let message = format!(
            "Valve acknowledgement timed out in {:?}: {}",
            self.status.phase,
            pending.join("; ")
        );
        match self.status.phase {
            // An operator cancel is already relieving; keep requesting it.
            Phase::CancelSetup => self.fault(input.now_ms, &message),
            Phase::PauseSetup => {
                let stuck_supply = [3, 4].into_iter().any(|i| {
                    !self.target[i]
                        && self.self_pending & (1 << i) == 0
                        && input.valves[i].is_some_and(|v| {
                            v.open
                                && v.at_ms >= self.self_ack_since[i]
                                && v.at_ms <= input.now_ms
                                && input.now_ms - v.at_ms <= 2000
                        })
                });
                let climbing = match (Self::fresh_pressure(input), self.pause_reference_psi) {
                    (Some(pressure), Some(reference)) => {
                        pressure >= reference + STUCK_SUPPLY_PRESSURE_RISE_PSI
                    }
                    _ => true,
                };
                if stuck_supply && climbing {
                    return self.fault(
                        input.now_ms,
                        &format!("{message}; a supply valve is stuck open while tank pressure climbs"),
                    );
                }
                // Keep closing and keep waiting for fresh confirmation.
                self.pause_for_failure(input.now_ms, &message)
            }
            _ => self.pause_for_failure(input.now_ms, &message),
        }
    }
    fn sample_mean(&self, since: u64) -> Option<f32> {
        let samples: Vec<_> = self.samples.iter().filter(|s| s.at_ms >= since).collect();
        if samples.len() < 3 {
            None
        } else {
            Some(samples.iter().map(|s| s.value as f64).sum::<f64>() as f32 / samples.len() as f32)
        }
    }
    pub fn tick(&mut self, input: Inputs) -> Effects {
        let new_sample = input.pressure.filter(|p| {
            p.value.is_finite() && self.samples.last().is_none_or(|last| p.at_ms > last.at_ms)
        });
        if let Some(sample) = new_sample {
            self.samples.push(sample);
        }
        self.samples
            .retain(|s| input.now_ms.saturating_sub(s.at_ms) <= 15000);
        if self.samples.len() > 2000 {
            self.samples.drain(..self.samples.len() - 2000);
        }
        self.status.pressure_psi = Self::fresh_pressure(input);
        self.status.valves = input.valves.map(|v| v.map(|v| v.open));
        if !self.active() {
            return Effects::default();
        }
        if matches!(
            self.status.phase,
            Phase::CancelSetup | Phase::PauseSetup | Phase::SelfFinish
        ) {
            // A pause that began without fresh pressure takes its reference
            // from the first fresh sample while closing.
            if self.status.phase == Phase::PauseSetup && self.pause_reference_psi.is_none() {
                self.pause_reference_psi = Self::fresh_pressure(input);
            }
            // Closing does not suspend the pressure ceiling.
            if self.status.phase == Phase::PauseSetup
                && let Some(pressure) = Self::fresh_pressure(input)
                && self
                    .session_pressure_limit()
                    .is_ok_and(|limit| pressure >= limit)
            {
                let effects = self.pressure_ceiling_reached(input.now_ms, pressure);
                if self.status.phase == Phase::Fault || effects.notification.is_some() {
                    return effects;
                }
            }
            if self.valve_ack_expired(input) {
                return self.valve_ack_timeout(input);
            }
            if self.self_confirmation_phase()
                && let Some(i) = self.self_waiting
                && input.valves[i].is_some_and(|v| {
                    v.open == self.target[i]
                        && v.at_ms > self.self_ack_since[i]
                        && v.at_ms <= input.now_ms
                        && input.now_ms - v.at_ms <= 2000
                })
            {
                self.self_waiting = None;
                if self.self_pending != 0 {
                    return self.next_self_command(input.now_ms);
                }
                self.phase_since = input.now_ms;
            }
            if self.confirmed(input) {
                let phase = match self.status.phase {
                    Phase::CancelSetup => Phase::Cancelled,
                    Phase::PauseSetup => Phase::Paused,
                    _ => Phase::Idle,
                };
                self.transition(
                    phase,
                    input.now_ms,
                    if phase == Phase::Idle {
                        "Valve self-test complete: dump and vent open"
                    } else if phase == Phase::Paused && self.paused_from.is_some() {
                        if self.testing_valves {
                            "Valve self-test paused: all valves closed"
                        } else {
                            "Nitrogen test paused: all valves closed"
                        }
                    } else if phase == Phase::Paused && self.failure.is_some() {
                        "Sequence paused after a failure: all valves closed"
                    } else if phase == Phase::Paused {
                        "Fill paused: all valves closed"
                    } else {
                        "Fill cancelled: dump and vent open"
                    },
                );
                if phase == Phase::Paused
                    && let Some(hold) = &self.test_hold
                {
                    self.status.message = hold.message();
                    return Effects {
                        notification: Some(self.status.message.clone()),
                        ..Effects::default()
                    };
                }
                let notification = match (phase, self.failure.as_deref()) {
                    (Phase::Paused, Some(reason)) => {
                        let reason = reason.to_string();
                        self.status.message = format!(
                            "{FAILURE_PAUSE_PREFIX} {reason}; all valves closed"
                        );
                        failure_pause_notice(&format!("{reason}; all valves are confirmed closed"))
                    }
                    _ => self.status.message.clone(),
                };
                return Effects {
                    notification: Some(notification),
                    ..Effects::default()
                };
            }
            if self.valve_ack_expired(input) {
                return self.valve_ack_timeout(input);
            }
            return Effects::default();
        }
        if !input.prelaunch {
            let mut effects =
                self.fault(input.now_ms, "Prelaunch state lost; supply close requested");
            effects.valves.retain(|(index, _)| *index >= 3);
            effects.notification = Some(
                "GSE automation stopped: vehicle left prelaunch state. Supply close requested."
                    .into(),
            );
            return effects;
        }
        if !input.interlock {
            return self.fault(input.now_ms, "Interlock lost");
        }
        let Some(pressure) = Self::fresh_pressure(input) else {
            return self.pause_or_hold_for_failure(
                input,
                "Tank pressure is missing, invalid or stale",
            );
        };
        let Ok(limit) = self.session_pressure_limit() else {
            return self.pause_or_hold_for_failure(
                input,
                "Pressure limit settings are invalid",
            );
        };
        if pressure >= limit {
            let effects = self.pressure_ceiling_reached(input.now_ms, pressure);
            if self.status.phase == Phase::Fault || effects.notification.is_some() {
                return effects;
            }
        } else {
            self.overfill_warned = false;
        }
        let setup = matches!(
            self.status.phase,
            Phase::BaselineSetup
                | Phase::SelfSetup
                | Phase::SelfOpen
                | Phase::SelfClose
                | Phase::NitrogenSetup
                | Phase::FillSetup
        );
        let awaiting_valves = setup
            || matches!(
                self.status.phase,
                Phase::Raising
                    | Phase::Settling
                    | Phase::Holding
                    | Phase::Dumping
                    | Phase::Filling
                    | Phase::Paused
            );
        if awaiting_valves && self.valve_ack_expired(input) {
            return self.valve_ack_timeout(input);
        }
        if self.self_confirmation_phase()
            && let Some(i) = self.self_waiting
            && input.valves[i].is_some_and(|v| {
                v.open == self.target[i]
                    && v.at_ms > self.self_ack_since[i]
                    && v.at_ms <= input.now_ms
            })
        {
            self.self_waiting = None;
            if self.self_pending != 0 {
                return self.next_self_command(input.now_ms);
            }
            self.phase_since = input.now_ms;
        }
        let elapsed = input.now_ms.saturating_sub(self.phase_since);
        match self.status.phase {
            Phase::BaselineSetup if self.confirmed(input) => {
                self.samples.clear();
                self.transition(
                    Phase::Baseline,
                    input.now_ms,
                    "Sampling empty-tank PT noise for 5 seconds",
                );
            }
            Phase::Baseline if elapsed >= 5000 => {
                if self.samples.len() < 10 {
                    return self.cancel_failed_test(
                        input.now_ms,
                        "Too few pressure samples for noise calibration",
                    );
                }
                let min = self
                    .samples
                    .iter()
                    .map(|s| s.value)
                    .fold(f32::INFINITY, f32::min);
                let max = self
                    .samples
                    .iter()
                    .map(|s| s.value)
                    .fold(f32::NEG_INFINITY, f32::max);
                let average = self.samples.iter().map(|s| s.value as f64).sum::<f64>() as f32
                    / self.samples.len() as f32;
                if self
                    .config
                    .maximum_zero_offset_psi
                    .is_none_or(|bound| min.abs().max(max.abs()) > bound)
                {
                    return self.fault(
                        input.now_ms,
                        "Empty-tank pressure exceeds the allowed zero offset",
                    );
                }
                self.status.baseline = Some(Baseline {
                    average_psi: average,
                    min_psi: min,
                    max_psi: max,
                    noise_psi: (average - min).max(max - average),
                    samples: self.samples.len(),
                });
                self.self_index = 0;
                self.status.current_step_psi = self.nitrogen_plan[0].target_psi;
                if self.testing_valves {
                    return self.configure_self(
                        Phase::SelfSetup,
                        RELIEVED,
                        input,
                        "Checking resting valve states before self-test",
                    );
                }
                return self.configure(
                    Phase::NitrogenSetup,
                    CLOSED,
                    input.now_ms,
                    "Closing all valves before testing",
                );
            }
            Phase::SelfSetup if self.confirmed(input) => {
                let mut target = RELIEVED;
                target[self.self_index] = !RELIEVED[self.self_index];
                return self.configure_self(
                    Phase::SelfOpen,
                    target,
                    input,
                    "Moving one valve for self-test",
                );
            }
            Phase::SelfOpen if self.confirmed(input) => self.transition(
                Phase::SelfHold,
                input.now_ms,
                "Valve self-test: two-second dwell",
            ),
            Phase::SelfHold if elapsed >= 2000 => {
                return self.configure_self(
                    Phase::SelfClose,
                    RELIEVED,
                    input,
                    "Restoring tested valve to its resting state",
                );
            }
            Phase::SelfClose if self.confirmed(input) => {
                self.self_index += 1;
                if self.self_index == 5 {
                    return self.configure_self(
                        Phase::SelfFinish,
                        RELIEVED,
                        input,
                        "Restoring dump and vent to open",
                    );
                }
                let mut target = RELIEVED;
                target[self.self_index] = !RELIEVED[self.self_index];
                return self.configure_self(
                    Phase::SelfOpen,
                    target,
                    input,
                    "Moving next valve for self-test",
                );
            }
            Phase::NitrogenSetup if self.confirmed(input) => {
                let mut target = CLOSED;
                target[3] = true;
                return self.configure(
                    Phase::Raising,
                    target,
                    input.now_ms,
                    "Raising nitrogen pressure to the next configured step",
                );
            }
            Phase::Raising => {
                if pressure
                    >= self.status.baseline.as_ref().unwrap().average_psi
                        + self.status.current_step_psi
                {
                    return self.configure(
                        Phase::Settling,
                        CLOSED,
                        input.now_ms,
                        "Nitrogen closed; waiting for pressure to settle",
                    );
                }
                if elapsed > 60000 {
                    return self.pause_for_failure(input.now_ms, "Nitrogen pressure step timed out");
                }
            }
            Phase::Settling => {
                if elapsed >= self.nitrogen_step().settle_ms
                    && self.confirmed(input)
                    && let Some(mean) = self.sample_mean(input.now_ms.saturating_sub(1000))
                {
                    let baseline = self.status.baseline.as_ref().unwrap();
                    if mean
                        < baseline.average_psi + self.status.current_step_psi
                            - 3.0
                            - baseline.noise_psi
                    {
                        return self.hold_test_pressure(
                            input.now_ms,
                            TestHold::Failed("Pressure fell below the test step while settling".into()),
                        );
                    }
                    self.hold_reference = Some(mean);
                    self.hold_samples = 0;
                    self.transition(Phase::Holding, input.now_ms, "Checking pressure hold");
                }
            }
            Phase::Holding => {
                if new_sample.is_some() {
                    self.hold_samples += 1;
                }
                if let Some(mean) = self.sample_mean(input.now_ms.saturating_sub(1000)) {
                    let noise = self.status.baseline.as_ref().unwrap();
                    if self.hold_reference.unwrap() - mean > 3.0 + 2.0 * noise.noise_psi {
                        return self.hold_test_pressure(
                            input.now_ms,
                            TestHold::Failed("Pressure drop exceeds 3 psi beyond the measured noise envelope".into()),
                        );
                    }
                }
                if elapsed >= self.nitrogen_step().hold_ms {
                    if self.hold_samples < 10 {
                        return self.pause_for_failure(
                            input.now_ms,
                            "Too few samples to verify pressure hold",
                        );
                    }
                    let hold_mean = self.sample_mean(self.phase_since).unwrap();
                    if self.hold_reference.unwrap() - hold_mean > 3.0 {
                        return self.hold_test_pressure(
                            input.now_ms,
                            TestHold::Failed(
                                "Sustained mean pressure loss exceeds 3 psi; leak or inconclusive noisy hold".into(),
                            ),
                        );
                    }
                    if self.nitrogen_step_index + 1 >= self.nitrogen_plan.len() {
                        return self.hold_test_pressure(input.now_ms, TestHold::Passed);
                    }
                    self.nitrogen_step_index += 1;
                    self.status.current_step_psi = self.nitrogen_step().target_psi;
                    return self.configure(
                        Phase::NitrogenSetup,
                        CLOSED,
                        input.now_ms,
                        "Preparing next pressure step",
                    );
                }
            }
            Phase::Dumping => {
                // The operator's Dump choice waits indefinitely for ~0 psi.
                if elapsed > 60000 && !self.dump_to_zero {
                    return self.fault(
                        input.now_ms,
                        "Tank did not return to its zero-pressure band",
                    );
                }
                let zero = self.status.baseline.as_ref().unwrap();
                let window: Vec<_> = self
                    .samples
                    .iter()
                    .filter(|s| s.at_ms >= input.now_ms.saturating_sub(1000))
                    .collect();
                let empty = elapsed >= 1000
                    && window.len() >= 10
                    && window
                        .first()
                        .is_some_and(|s| input.now_ms.saturating_sub(s.at_ms) >= 900)
                    && window.iter().all(|s| zero.is_zero(s.value));
                if self.confirmed(input) && empty && zero.is_zero(pressure) {
                    let failed = matches!(self.test_hold, Some(TestHold::Failed(_)));
                    self.test_hold = None;
                    self.dump_to_zero = false;
                    if failed {
                        self.failure = None;
                        self.status.nitrogen_passed = false;
                        self.transition(
                            Phase::Cancelled,
                            input.now_ms,
                            "Nitrogen test failed; tank vented to its zero-pressure band",
                        );
                        return Effects {
                            notification: Some(self.status.message.clone()),
                            ..Effects::default()
                        };
                    }
                    self.status.nitrogen_passed = true;
                    self.transition(
                        Phase::Passed,
                        input.now_ms,
                        "Nitrogen test passed; tank empty; fill unlocked",
                    );
                    return Effects {
                        notification: Some(self.status.message.clone()),
                        ..Effects::default()
                    };
                }
            }
            Phase::FillSetup if self.confirmed(input) => {
                if !self.fill_entry_pressure_ready(pressure, self.resume_existing_inventory) {
                    return self.pause_for_failure(
                        input.now_ms,
                        "Tank is not within the calibrated zero-pressure band",
                    );
                }
                let mut target = FILL_READY;
                target[4] = true;
                return self.configure(
                    Phase::Filling,
                    target,
                    input.now_ms,
                    "Nitrous fill in progress",
                );
            }
            _ => {}
        }
        Effects::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_defaults_use_requested_targets_without_inventing_hardware_limits() {
        let config = Config::default();
        let plan = config.nitrogen_plan().unwrap();
        assert_eq!(config.nitrogen_target_psi, 500.0);
        assert_eq!(plan.len(), 10);
        assert_eq!(plan.first().unwrap().target_psi, 50.0);
        assert_eq!(plan.last().unwrap().target_psi, 500.0);
        assert_eq!(Engine::default().nitrous_target_psi, 750.0);
        assert!(config.pressure_ceiling_psi.is_none());
        assert!(config.turbocharge_max_added_mass_kg.is_none());
        assert!(config.cold_flow_target_psi.is_none());
    }
    #[test]
    fn saved_low_pressure_limits_are_preserved_and_missing_target_requires_review() {
        let complete: Config = serde_json::from_str(
            r#"{"nitrogen_target_psi":120,"pressure_ceiling_psi":200,"maximum_zero_offset_psi":8}"#,
        )
        .unwrap();
        assert_eq!(complete.nitrogen_target_psi, 120.0);
        assert_eq!(complete.pressure_ceiling_psi, Some(200.0));
        assert!(complete.validate_settings().is_ok());
        let partial: Config =
            serde_json::from_str(r#"{"pressure_ceiling_psi":200,"maximum_zero_offset_psi":8}"#)
                .unwrap();
        assert_eq!(partial.nitrogen_target_psi, 500.0);
        assert_eq!(partial.pressure_ceiling_psi, Some(200.0));
        assert_eq!(partial.maximum_zero_offset_psi, Some(8.0));
        assert!(
            partial.validate_settings().is_err(),
            "missing legacy target must not raise the saved hardware ceiling"
        );
    }
    struct Rig {
        engine: Engine,
        now: u64,
        pressure: f32,
        valves: [bool; 5],
    }
    impl Rig {
        fn new() -> Self {
            let engine = Engine {
                config: Config {
                    cold_flow_target_psi: None,
                    nitrogen_target_psi: 120.0,
                    nitrogen_steps: Vec::new(),
                    fill_overfill_kg: 0.05,
                    fill_tolerance_kg: 0.07,
                    fill_settle_ms: 3000,
                    turbocharge_max_added_mass_kg: None,
                    turbocharge_pressure_deadband_psi: 12.,
                    pressure_step_psi: 50.0,
                    pressure_ceiling_psi: Some(200.0),
                    maximum_zero_offset_psi: Some(8.0),
                    dry_self_test_confirmed: true,
                    grouped_panel: true,
                    ..Config::default()
                },
                ..Engine::default()
            };
            Self {
                engine,
                now: 100,
                pressure: 5.0,
                valves: RELIEVED,
            }
        }
        fn input(&self) -> Inputs {
            Inputs {
                now_ms: self.now,
                pressure: Some(Sample {
                    value: self.pressure,
                    at_ms: self.now,
                }),
                valves: std::array::from_fn(|i| {
                    Some(ValveState {
                        open: self.valves[i],
                        at_ms: self.now,
                    })
                }),
                interlock: true,
                prelaunch: true,
            }
        }
        fn effects(&mut self, effects: Effects) {
            for (i, open) in effects.valves {
                self.valves[i] = open;
            }
        }
        fn request(&mut self, action: Action) {
            let effects = self.engine.request(action, self.input()).unwrap();
            self.effects(effects);
        }
        fn tick(&mut self) {
            self.now += 100;
            let effects = self.engine.tick(self.input());
            self.effects(effects);
        }
        fn until(&mut self, phase: Phase) {
            for _ in 0..1000 {
                if self.engine.status.phase == phase
                    && self.engine.self_pending == 0
                    && self.engine.self_waiting.is_none()
                {
                    return;
                }
                self.tick();
            }
            panic!("did not reach {phase:?}: {:?}", self.engine.status);
        }
        fn baseline(&mut self) {
            self.request(Action::NitrogenTest);
            self.until(Phase::Baseline);
            for i in 0..50 {
                self.pressure = if i % 2 == 0 { 3.0 } else { 7.0 };
                self.tick();
            }
            self.pressure = 5.0;
        }
    }
    #[test]
    fn startup_relief_and_filled_restart_do_not_replay_a_sequence() {
        for (hold_filled, desired, settled) in [
            (false, RELIEVED, Phase::Cancelled),
            (true, CLOSED, Phase::Paused),
        ] {
            let mut rig = Rig::new();
            rig.valves = std::array::from_fn(|i| !desired[i]);
            let effects = rig.engine.prepare_startup(rig.now, hold_filled);
            rig.effects(effects);
            rig.until(settled);
            assert_eq!(rig.valves, desired);
            for _ in 0..100 {
                rig.tick();
                assert_eq!(rig.engine.status.phase, settled);
                assert_eq!(rig.valves, desired);
            }
        }
    }
    #[test]
    fn missing_limits_prevent_automatic_actions() {
        assert!(Config::default().validate().is_err());
        let mut rig = Rig::new();
        rig.engine.config.pressure_ceiling_psi = None;
        assert!(rig.engine.allows(Action::NitrogenTest, rig.input()));
        rig.engine.config.maximum_zero_offset_psi = None;
        assert!(!rig.engine.allows(Action::NitrogenTest, rig.input()));
    }
    #[test]
    fn relative_pressure_limits_fault_at_boundary_and_close_supplies() {
        for (nitrous, target, margin) in [(false, 120.0, 100.0), (true, 745.0, 50.0)] {
            let mut rig = Rig::new();
            rig.engine.config.pressure_ceiling_psi = None;
            rig.engine.nitrous_session = nitrous;
            rig.engine.nitrous_target_psi = target;
            rig.engine.status.phase = Phase::Paused;
            rig.pressure = target + margin - 0.5;
            rig.tick();
            assert_eq!(rig.engine.status.phase, Phase::Paused);
            rig.pressure = target + margin;
            rig.tick();
            assert_eq!(rig.engine.status.phase, Phase::Fault);
            assert_eq!(rig.valves, RELIEVED);
        }
    }
    #[test]
    fn lower_hardware_cap_and_invalid_targets_cannot_be_bypassed() {
        let config = Config {
            pressure_ceiling_psi: Some(180.0),
            ..Config::default()
        };
        assert_eq!(config.pressure_limit(120.0, 100.0).unwrap(), 180.0);
        assert_eq!(config.pressure_limit(745.0, 50.0).unwrap(), 180.0);
        for target in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            assert!(config.pressure_limit(target, 50.0).is_err());
        }
        let mut rig = Rig::new();
        rig.engine.nitrous_session = true;
        rig.engine.status.phase = Phase::Filling;
        rig.pressure = 200.0;
        rig.tick();
        assert_eq!(rig.engine.status.phase, Phase::Fault);
        assert_eq!(rig.valves, RELIEVED);
    }
    #[test]
    fn zero_centered_three_psi_noise_is_measured() {
        let mut rig = Rig::new();
        rig.engine.config.maximum_zero_offset_psi = Some(3.0);
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        rig.until(Phase::Baseline);
        for i in 0..50 {
            rig.pressure = if i % 2 == 0 { -3.0 } else { 3.0 };
            rig.tick();
        }
        let baseline = rig.engine.status.baseline.as_ref().unwrap();
        assert_eq!(baseline.average_psi, 0.0);
        assert_eq!(baseline.noise_psi, 3.0);
        assert!(baseline.is_zero(-3.0) && baseline.is_zero(3.0));
    }
    #[test]
    fn interlock_loss_and_pressure_ceiling_close_supplies() {
        for lose_interlock in [false, true] {
            let mut rig = Rig::new();
            rig.baseline();
            rig.until(Phase::Raising);
            let mut input = rig.input();
            if lose_interlock {
                input.interlock = false;
            } else {
                input.pressure.as_mut().unwrap().value = 200.0;
            }
            let effects = rig.engine.tick(input);
            rig.effects(effects);
            assert_eq!(rig.engine.status.phase, Phase::Fault);
            assert_eq!(rig.valves, RELIEVED);
            assert!(!rig.engine.status.nitrogen_passed);
        }
    }
    #[test]
    fn sustained_small_drop_inside_extrema_envelope_cannot_pass() {
        let mut rig = Rig::new();
        rig.baseline();
        rig.until(Phase::Raising);
        rig.pressure = 55.0;
        rig.until(Phase::Holding);
        rig.pressure = 51.0;
        rig.until(Phase::Paused);
        assert!(!rig.engine.status.nitrogen_passed);
        assert_eq!(rig.valves, CLOSED);
        rig.request(Action::CancelFill);
        rig.pressure = 5.0;
        rig.until(Phase::Cancelled);
        assert_eq!(rig.valves, RELIEVED);
        assert!(!rig.engine.active());
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        assert_eq!(rig.engine.status.phase, Phase::BaselineSetup);
    }
    #[test]
    fn overpressure_thresholds_default_and_validate_against_tank_rating() {
        let config = Config::default();
        assert_eq!(
            (config.overpressure_relief_psi, config.overpressure_abort_psi, config.overpressure_pilot_ms),
            (850.0, 950.0, 3000)
        );
        assert!(config.validate_overpressure().is_ok());
        let legacy: Config = serde_json::from_str(r#"{"nitrogen_target_psi":120}"#).unwrap();
        assert_eq!(legacy.overpressure_relief_psi, 850.0);
        for (relief, abort, pilot, rating) in [
            (950.0, 950.0, 3000, None),
            (960.0, 950.0, 3000, None),
            (f32::NAN, 950.0, 3000, None),
            (0.0, 950.0, 3000, None),
            (850.0, 950.0, 0, None),
            (850.0, 950.0, 3000, Some(900.0)),
            (850.0, 950.0, 3000, Some(950.0)),
        ] {
            let bad = Config {
                overpressure_relief_psi: relief,
                overpressure_abort_psi: abort,
                overpressure_pilot_ms: pilot,
                tank_rating_psi: rating,
                ..Config::default()
            };
            assert!(bad.validate_overpressure().is_err(), "{relief} {abort} {pilot} {rating:?}");
            assert!(bad.validate_settings().is_err());
        }
        let rated = Config { tank_rating_psi: Some(1200.0), ..Config::default() };
        assert!(rated.validate_overpressure().is_ok());
    }
    #[test]
    fn incomplete_settings_can_select_manual_panel_without_enabling_automation() {
        let config = Config {
            grouped_panel: false,
            ..Config::default()
        };
        assert!(config.validate_settings().is_ok());
        assert!(config.validate().is_err());
    }
    #[test]
    fn noise_floor_keeps_average_and_extrema() {
        let mut rig = Rig::new();
        rig.baseline();
        let noise = rig.engine.status.baseline.as_ref().unwrap();
        assert!((noise.average_psi - 5.0).abs() < 0.01);
        assert_eq!(noise.min_psi, 3.0);
        assert_eq!(noise.max_psi, 7.0);
        assert!(noise.is_zero(3.0) && noise.is_zero(7.0));
        assert!(!noise.is_zero(8.0));
    }
    #[test]
    fn elevated_baseline_cannot_be_learned_as_zero() {
        let mut rig = Rig::new();
        rig.pressure = 30.0;
        rig.request(Action::NitrogenTest);
        rig.until(Phase::Fault);
        assert!(!rig.engine.status.nitrogen_passed);
        assert_eq!(rig.valves, RELIEVED);
    }
    #[test]
    fn nitrogen_steps_hold_and_dump_before_unlocking_fill() {
        let mut rig = Rig::new();
        rig.baseline();
        for target in [50.0, 100.0, 120.0] {
            rig.until(Phase::Raising);
            assert_eq!(rig.engine.status.current_step_psi, target);
            assert_eq!(rig.valves, [false, false, false, true, false]);
            rig.pressure = 5.0 + target;
            rig.until(Phase::Holding);
            assert_eq!(rig.valves, CLOSED);
            for _ in 0..49 {
                rig.tick();
                assert!(!rig.engine.status.nitrogen_passed);
            }
            rig.tick();
        }
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        rig.until(Phase::Paused);
        assert_eq!(rig.valves, CLOSED, "holds the top-step pressure");
        rig.request(Action::CancelFill);
        rig.until(Phase::Dumping);
        assert_eq!(rig.valves, RELIEVED);
        rig.pressure = 5.0;
        rig.until(Phase::Passed);
        assert!(rig.engine.status.nitrogen_passed);
        assert!(!rig.engine.allows(Action::SelfTest, rig.input()));
        // This fixture has a 200 psi hardware cap, so use a compatible fill target.
        rig.engine.nitrous_target_psi = 140.0;
        rig.request(Action::StartFill);
        rig.until(Phase::Filling);
        assert_eq!(rig.valves, [false, true, false, false, true]);
        rig.request(Action::PauseFill);
        rig.until(Phase::Paused);
        assert_eq!(rig.valves, CLOSED);
        rig.request(Action::CancelFill);
        rig.until(Phase::Cancelled);
        assert_eq!(rig.valves, RELIEVED);
    }
    #[test]
    fn nitrogen_pt_overshoot_settles_then_holds_before_each_next_level() {
        let mut rig = Rig::new();
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        rig.until(Phase::Raising);
        for target in [50.0, 100.0, 120.0] {
            assert_eq!(rig.engine.status.current_step_psi, target);
            assert!(rig.valves[3]);
            // A real PT trace crosses the target between samples. Closing the
            // supply is immediate; the overshoot must not become a leak baseline.
            rig.pressure = target + 4.0;
            rig.tick();
            assert_eq!(rig.engine.status.phase, Phase::Settling);
            assert_eq!(rig.valves, CLOSED);
            for index in 0..19 {
                rig.pressure = target
                    + if index < 5 {
                        3.5 - index as f32 * 0.3
                    } else {
                        2.0
                    };
                rig.tick();
                assert_eq!(rig.engine.status.phase, Phase::Settling);
                assert_eq!(rig.engine.status.current_step_psi, target);
            }
            rig.pressure = target + 2.0;
            rig.until(Phase::Holding);
            assert_eq!(rig.engine.status.phase, Phase::Holding);
            let reference = rig.engine.hold_reference.unwrap();
            assert!((reference - (target + 2.0)).abs() < 0.1);
            for index in 0..49 {
                rig.pressure = target + 2.0 + if index % 2 == 0 { 0.1 } else { -0.1 };
                rig.tick();
                assert_eq!(rig.engine.status.phase, Phase::Holding);
                assert_eq!(rig.valves, CLOSED);
                assert_eq!(rig.engine.status.current_step_psi, target);
                assert!(!rig.engine.status.nitrogen_passed);
            }
            rig.tick();
            if target < 120.0 {
                assert_eq!(rig.engine.status.phase, Phase::NitrogenSetup);
                assert_eq!(rig.valves, CLOSED);
                rig.until(Phase::Raising);
                assert_eq!(rig.engine.status.phase, Phase::Raising);
            }
        }
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        rig.until(Phase::Paused);
        rig.request(Action::CancelFill);
        rig.pressure = 0.0;
        rig.until(Phase::Passed);
        assert!(rig.engine.status.nitrogen_passed);
    }

    #[test]
    fn nitrogen_pt_continuing_leak_after_overshoot_never_advances() {
        let mut rig = Rig::new();
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        rig.until(Phase::Raising);
        rig.pressure = 54.0;
        rig.tick();
        rig.pressure = 52.0;
        rig.until(Phase::Holding);
        for index in 1..=50 {
            rig.pressure = 52.0 - index as f32 * 0.15;
            rig.tick();
            assert_eq!(rig.engine.status.current_step_psi, 50.0);
            if matches!(
                rig.engine.status.phase,
                Phase::PauseSetup | Phase::Paused
            ) {
                break;
            }
        }
        rig.until(Phase::Paused);
        assert_eq!(rig.valves, CLOSED);
        rig.request(Action::CancelFill);
        rig.pressure = 0.0;
        rig.until(Phase::Cancelled);
        assert_eq!(rig.engine.status.phase, Phase::Cancelled);
        assert!(!rig.engine.status.nitrogen_passed);
        assert_eq!(rig.valves, RELIEVED);
    }

    #[test]
    fn configured_pressure_steps_are_used_and_final_step_is_capped() {
        let mut rig = Rig::new();
        rig.engine.config.pressure_step_psi = 40.0;
        rig.engine.config.nitrogen_target_psi = 90.0;
        rig.baseline();
        for target in [40.0, 80.0, 90.0] {
            rig.until(Phase::Raising);
            assert_eq!(rig.engine.status.current_step_psi, target);
            rig.pressure = 5.0 + target;
            rig.until(Phase::Holding);
            for _ in 0..50 {
                rig.tick();
            }
        }
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert_eq!(rig.engine.test_hold(), Some(&TestHold::Passed));
    }
    #[test]
    fn pressure_step_must_be_finite_and_positive() {
        for step in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            let mut cfg = Rig::new().engine.config;
            cfg.pressure_step_psi = step;
            assert!(cfg.validate_settings().is_err());
            assert!(cfg.validate().is_err());
        }
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.pressure_step_psi, 50.0);
    }
    #[test]
    fn leak_beyond_noise_fails_and_closes_supplies() {
        let mut rig = Rig::new();
        rig.baseline();
        rig.until(Phase::Raising);
        rig.pressure = 55.0;
        rig.until(Phase::Holding);
        rig.pressure = 45.0;
        rig.until(Phase::Paused);
        assert_eq!(rig.valves, CLOSED);
        rig.request(Action::CancelFill);
        rig.pressure = 5.0;
        rig.until(Phase::Cancelled);
        assert_eq!(rig.valves, RELIEVED);
        assert!(!rig.engine.active());
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        assert_eq!(rig.engine.status.phase, Phase::BaselineSetup);
        assert!(!rig.engine.status.nitrogen_passed);
    }
    #[test]
    fn noise_only_does_not_fail_hold() {
        let mut rig = Rig::new();
        rig.baseline();
        rig.until(Phase::Raising);
        rig.pressure = 55.0;
        rig.until(Phase::Holding);
        for i in 0..50 {
            rig.pressure = if i % 2 == 0 { 53.0 } else { 57.0 };
            rig.tick();
            assert_ne!(rig.engine.status.phase, Phase::Fault);
        }
    }
    #[test]
    fn stale_pressure_pauses_closed_instead_of_reporting_pass() {
        let mut rig = Rig::new();
        rig.baseline();
        rig.until(Phase::Raising);
        let mut input = rig.input();
        input.pressure.as_mut().unwrap().at_ms = 0;
        let effects = rig.engine.tick(input);
        assert_eq!(effects.valves, vec![(3, false), (4, false)]);
        assert!(effects.notification.as_deref().unwrap().starts_with("Sequence paused: Tank pressure"));
        rig.effects(effects);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert_eq!(rig.valves, CLOSED);
        assert!(!rig.engine.status.nitrogen_passed);
    }
    #[test]
    fn missing_valve_ack_never_opens_supply() {
        let mut rig = Rig::new();
        rig.request(Action::NitrogenTest);
        let mut input = rig.input();
        input.now_ms += 5100;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        input.valves = [None; 5];
        let effects = rig.engine.tick(input);
        assert!(!effects.valves.iter().any(|(_, open)| *open));
        rig.effects(effects);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert!(rig.engine.failure().is_some());
        assert!(!rig.valves[3] && !rig.valves[4]);
    }
    #[test]
    fn valve_timeout_identifies_missing_stale_and_wrong_reports() {
        let mut rig = Rig::new();
        rig.now = 100;
        rig.request(Action::SelfTest);
        let mut input = rig.input();
        input.now_ms += 5100;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        input.valves[0] = None;
        input.valves[1] = Some(ValveState {
            open: true,
            at_ms: 0,
        });
        input.valves[2] = Some(ValveState {
            open: false,
            at_ms: input.now_ms,
        });
        input.valves[3] = Some(ValveState {
            open: false,
            at_ms: input.now_ms,
        });
        input.valves[4] = Some(ValveState {
            open: false,
            at_ms: input.now_ms,
        });
        let effects = rig.engine.tick(input);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert!(!effects.valves.iter().any(|(_, open)| *open));
        let message = effects.notification.unwrap();
        assert!(message.starts_with("Sequence paused: Valve acknowledgement timed out"));
        assert!(message.ends_with("Review and Resume or Cancel."));
        assert!(message.contains("Pilot closed: no report"));
        assert!(message.contains("Vent open: no fresh report"));
        assert!(message.contains("Dump open: opposite state reported"));
        assert!(!message.contains("Nitrogen closed:"));
    }
    #[test]
    fn self_test_uses_recent_resting_reports_and_only_confirms_changed_valves() {
        let mut rig = Rig::new();
        let mut input = rig.input();
        let effects = rig.engine.request(Action::SelfTest, input).unwrap();
        // The normal vent/dump-open state requires no initial movement.
        assert!(effects.valves.is_empty());
        let mut commands = Vec::new();
        for _ in 0..300 {
            input.now_ms += 100;
            input.pressure.as_mut().unwrap().at_ms = input.now_ms;
            // Boards publish full physical snapshots every second, even if no valve changed.
            if input.now_ms.is_multiple_of(1000) {
                for state in input.valves.iter_mut().flatten() {
                    state.at_ms = input.now_ms;
                }
            }
            let effects = rig.engine.tick(input);
            assert_ne!(rig.engine.status.phase, Phase::Fault);
            assert!(effects.valves.len() <= 1);
            for (i, open) in effects.valves {
                commands.push((i, open));
                // A command also produces its own fresh physical reply.
                input.valves[i] = Some(ValveState {
                    open,
                    at_ms: input.now_ms,
                });
            }
            if rig.engine.status.phase == Phase::Idle {
                break;
            }
        }
        assert_eq!(rig.engine.status.phase, Phase::Idle);
        assert_eq!(
            commands,
            vec![
                (0, true),
                (0, false),
                (1, false),
                (1, true),
                (2, false),
                (2, true),
                (3, true),
                (3, false),
                (4, true),
                (4, false)
            ]
        );
    }
    #[test]
    fn self_test_waits_for_each_unknown_resting_state_and_pauses_without_reply() {
        let mut rig = Rig::new();
        let mut input = rig.input();
        input.valves = [None; 5];
        let effects = rig.engine.request(Action::SelfTest, input).unwrap();
        assert_eq!(effects.valves, vec![(3, false)]);
        input.now_ms += 100;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        assert!(rig.engine.tick(input).valves.is_empty());
        input.valves[3] = Some(ValveState {
            open: false,
            at_ms: input.now_ms,
        });
        assert_eq!(rig.engine.tick(input).valves, vec![(4, false)]);
        input.now_ms += 5100;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        let effects = rig.engine.tick(input);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert!(!effects.valves.iter().any(|(_, open)| *open));
        assert!(
            effects
                .notification
                .unwrap()
                .contains("Nitrous closed: no report")
        );
        assert!(effects.valves.contains(&(3, false)));
        assert!(effects.valves.contains(&(4, false)));
    }
    #[test]
    fn self_test_gives_each_dispatched_valve_its_own_five_seconds() {
        let mut rig = Rig::new();
        let mut input = rig.input();
        input.valves = [None; 5];
        assert_eq!(
            rig.engine.request(Action::SelfTest, input).unwrap().valves,
            vec![(3, false)]
        );
        input.now_ms += 4000;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        input.valves[3] = Some(ValveState {
            open: false,
            at_ms: input.now_ms,
        });
        assert_eq!(rig.engine.tick(input).valves, vec![(4, false)]);
        input.now_ms += 4999;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        input.valves[3].as_mut().unwrap().at_ms = input.now_ms;
        assert!(rig.engine.tick(input).valves.is_empty());
        assert_ne!(rig.engine.status.phase, Phase::Fault);
        input.now_ms += 1;
        input.pressure.as_mut().unwrap().at_ms = input.now_ms;
        let effects = rig.engine.tick(input);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert!(!effects.valves.iter().any(|(_, open)| *open));
    }
    #[test]
    fn every_automatic_valve_phase_uses_the_same_ack_deadline() {
        for phase in [
            Phase::BaselineSetup,
            Phase::NitrogenSetup,
            Phase::FillSetup,
            Phase::Raising,
            Phase::Settling,
            Phase::Holding,
            Phase::Dumping,
            Phase::Filling,
            Phase::PauseSetup,
            Phase::CancelSetup,
            Phase::Paused,
        ] {
            let mut rig = Rig::new();
            let mut input = rig.input();
            rig.engine.configure(phase, CLOSED, input.now_ms, "test");
            input.valves = [None; 5];
            input.now_ms += 4999;
            assert!(!rig.engine.valve_ack_expired(input), "{phase:?}");
            input.now_ms += 1;
            assert!(rig.engine.valve_ack_expired(input), "{phase:?}");
        }
    }
    #[test]
    fn self_test_exercises_one_valve_at_a_time_and_restores_normally_open_valves() {
        let mut rig = Rig::new();
        rig.request(Action::SelfTest);
        rig.until(Phase::SelfHold);
        let mut seen = Vec::new();
        for _ in 0..300 {
            if rig.engine.status.phase == Phase::SelfHold {
                assert_eq!(
                    rig.valves
                        .iter()
                        .zip(RELIEVED)
                        .filter(|(v, normal)| **v != *normal)
                        .count(),
                    1
                );
                let valve = rig
                    .valves
                    .iter()
                    .zip(RELIEVED)
                    .position(|(v, normal)| *v != normal)
                    .unwrap();
                if !seen.contains(&valve) {
                    seen.push(valve);
                }
            }
            rig.tick();
            if rig.engine.status.phase == Phase::Idle {
                break;
            }
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
        assert_eq!(rig.valves, RELIEVED);
        assert_eq!(rig.engine.status.phase, Phase::Idle);
    }
    #[test]
    fn explicit_nitrogen_steps_survive_serialization_and_use_each_wait() {
        let mut rig = Rig::new();
        rig.engine.config.pressure_ceiling_psi = Some(700.0);
        rig.engine.config.nitrogen_steps = vec![
            NitrogenStep {
                target_psi: 100.0,
                settle_ms: 3000,
                hold_ms: 1000,
            },
            NitrogenStep {
                target_psi: 300.0,
                settle_ms: 1000,
                hold_ms: 2000,
            },
            NitrogenStep {
                target_psi: 500.0,
                settle_ms: 2000,
                hold_ms: 3000,
            },
        ];
        let json = serde_json::to_string(&rig.engine.config).unwrap();
        assert_eq!(
            serde_json::from_str::<Config>(&json).unwrap(),
            rig.engine.config
        );
        assert_eq!(rig.engine.config.nitrogen_target_psi(), 500.0);
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        for step in rig.engine.config.nitrogen_steps.clone() {
            rig.until(Phase::Raising);
            assert_eq!(rig.engine.status.current_step_psi, step.target_psi);
            rig.pressure = step.target_psi;
            rig.until(Phase::Settling);
            let settled_at = rig.engine.phase_since;
            rig.until(Phase::Holding);
            assert!(rig.now - settled_at >= step.settle_ms);
            let held_at = rig.now;
            while rig.engine.status.phase == Phase::Holding {
                rig.tick();
            }
            assert!(rig.now - held_at >= step.hold_ms);
            assert_ne!(rig.engine.status.phase, Phase::Fault);
        }
        rig.until(Phase::Paused);
        rig.request(Action::CancelFill);
        rig.pressure = 0.0;
        rig.until(Phase::Passed);
        assert!(rig.engine.status.nitrogen_passed);
    }
    #[test]
    fn explicit_steps_and_fill_settings_reject_unsafe_bounds() {
        let mut config = Config::default();
        let good = NitrogenStep {
            target_psi: 100.0,
            settle_ms: 1000,
            hold_ms: 1000,
        };
        for bad in [
            NitrogenStep {
                target_psi: f32::NAN,
                ..good.clone()
            },
            NitrogenStep {
                settle_ms: 0,
                ..good.clone()
            },
            NitrogenStep {
                hold_ms: 600001,
                ..good.clone()
            },
        ] {
            config.nitrogen_steps = vec![bad];
            assert!(config.validate_settings().is_err());
        }
        config.nitrogen_steps = vec![good.clone(), good];
        assert!(config.validate_settings().is_err());
        config.nitrogen_steps = (1..=33)
            .map(|i| NitrogenStep {
                target_psi: i as f32,
                settle_ms: 1000,
                hold_ms: 1000,
            })
            .collect();
        assert!(config.validate_settings().is_err());
        config.nitrogen_steps = (1..=4)
            .map(|i| NitrogenStep {
                target_psi: i as f32,
                settle_ms: 600000,
                hold_ms: 600000,
            })
            .collect();
        assert!(config.validate_settings().is_err());
        config.nitrogen_steps.clear();
        config.fill_overfill_kg = 0.101;
        assert!(config.validate_settings().is_err());
        config.fill_overfill_kg = 0.1;
        config.fill_tolerance_kg = 0.071;
        assert!(config.validate_settings().is_err());
        config.fill_tolerance_kg = 0.07;
        config.fill_settle_ms = 999;
        assert!(config.validate_settings().is_err());
    }
    #[test]
    fn ordinary_targets_wait_for_board_acks_and_settle_after_last_ack() {
        let mut rig = Rig::new();
        rig.engine.status.phase = Phase::Raising;
        let mut effects = rig
            .engine
            .configure(Phase::Settling, CLOSED, rig.now, "settle");
        for index in [3, 4, 0, 1, 2] {
            assert_eq!(effects.valves, vec![(index, false)]);
            // Old cached CLOSED feedback cannot release the next command.
            let mut stale = rig.input();
            stale.valves[index].as_mut().unwrap().at_ms =
                rig.engine.self_ack_since[index].saturating_sub(1);
            assert!(rig.engine.tick(stale).valves.is_empty());
            rig.effects(effects);
            rig.now += 1000;
            effects = rig.engine.tick(rig.input());
        }
        assert_eq!(rig.engine.status.phase, Phase::Settling);
        assert_eq!(rig.engine.phase_since, rig.now);
        assert!(effects.valves.is_empty());
    }
    #[test]
    fn manual_override_cancels_automation_without_clearing_safety_or_test_evidence() {
        let mut rig = Rig::new();
        rig.engine.status.nitrogen_passed = true;
        rig.engine.status.self_test_locked = true;
        rig.engine.nitrous_session = true;
        let _ = rig
            .engine
            .configure(Phase::FillSetup, FILL_READY, rig.now, "fill");
        rig.engine.manual_override();
        assert!(
            rig.engine.status.nitrogen_passed
                && rig.engine.status.self_test_locked
                && rig.engine.nitrous_session()
        );
        for _ in 0..100 {
            rig.now += 100;
            assert!(rig.engine.tick(rig.input()).valves.is_empty());
        }
        rig.engine.fault(rig.now, "latched");
        rig.engine.manual_override();
        assert_eq!(rig.engine.status.phase, Phase::Fault);
    }
    #[test]
    fn previously_acked_but_now_stale_board_reports_cannot_sustain_automation() {
        let mut rig = Rig::new();
        rig.engine.status.phase = Phase::Holding;
        rig.engine.target = CLOSED;
        rig.engine.self_ack_since = [100; 5];
        rig.now = 10000;
        let mut input = rig.input();
        input.valves = input.valves.map(|v| {
            v.map(|mut v| {
                v.open = false;
                v.at_ms = 7000;
                v
            })
        });
        let effects = rig.engine.tick(input);
        assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
        assert_eq!(effects.valves, vec![(3, false), (4, false)]);
        let message = effects.notification.unwrap();
        assert!(message.starts_with("Sequence paused:"));
        assert!(message.contains("no fresh report"));
    }
    #[test]
    fn progress_getter_preserves_actual_sequence_outputs_and_transitions() {
        for action in [Action::SelfTest,Action::NitrogenTest] {
            let mut control=Rig::new();let mut observed=Rig::new();
            control.request(action);observed.request(action);
            let mut finished=false;
            for _ in 0..1000 {
                let pressure=match control.engine.status.phase {
                    Phase::Raising|Phase::Settling|Phase::Holding=>control.engine.status.baseline.as_ref().map_or(5.,|b|b.average_psi)+control.engine.status.current_step_psi,
                    _=>5.,
                };
                control.pressure=pressure;observed.pressure=pressure;
                for _ in 0..3 {
                    let view=observed.engine.progress_observation(observed.input());
                    assert_eq!(view.phase,observed.engine.status.phase);
                    assert_eq!(view.all_valves_confirmed_by_engine,observed.engine.confirmed(observed.input()));
                    assert_eq!(view.fresh_pressure_psi,Engine::fresh_pressure(observed.input()));
                }
                if control.engine.status.phase==Phase::Paused&&control.engine.test_hold().is_some(){
                    control.request(Action::CancelFill);observed.request(Action::CancelFill);
                }
                control.now+=100;observed.now+=100;
                let expected=control.engine.tick(control.input());
                let actual=observed.engine.tick(observed.input());
                assert_eq!(expected.valves,actual.valves);assert_eq!(expected.notification,actual.notification);
                control.effects(expected);observed.effects(actual);
                assert_eq!(serde_json::to_value(&control.engine.status).unwrap(),serde_json::to_value(&observed.engine.status).unwrap());
                assert_eq!(control.engine.phase_since,observed.engine.phase_since);
                assert_eq!(control.engine.command_since,observed.engine.command_since);
                assert_eq!(control.engine.samples.len(),observed.engine.samples.len());
                assert_eq!(control.engine.self_index,observed.engine.self_index);
                assert_eq!(control.engine.nitrogen_step_index,observed.engine.nitrogen_step_index);
                if matches!(action,Action::SelfTest)&&control.engine.status.phase==Phase::Idle
                    || matches!(action,Action::NitrogenTest)&&control.engine.status.phase==Phase::Passed {
                    finished=true;break;
                }
                assert_ne!(control.engine.status.phase,Phase::Fault);
            }
            assert!(finished,"Actual {action:?} sequence did not reach its actual engine terminal phase: {:?}",control.engine.status);
        }
    }
    #[test]
    fn progress_getter_keeps_missing_stale_future_and_nonfinite_inputs_unknown() {
        let engine=Engine::default();
        let mut input=Inputs{now_ms:3000,pressure:None,valves:[None;5],interlock:true,prelaunch:true};
        let view=engine.progress_observation(input);assert!(view.fresh_pressure_psi.is_none());assert!(view.valves.iter().all(|v|v.fresh_readback_open.is_none()));
        input.pressure=Some(Sample{value:f32::NAN,at_ms:3000});input.valves[0]=Some(ValveState{open:true,at_ms:999});
        let view=engine.progress_observation(input);assert!(view.fresh_pressure_psi.is_none());assert_eq!(view.valves[0].raw_readback_open,Some(true));assert_eq!(view.valves[0].fresh_readback_open,None);
        input.pressure=Some(Sample{value:0.,at_ms:3001});input.valves[0]=Some(ValveState{open:true,at_ms:3001});
        let view=engine.progress_observation(input);assert!(view.fresh_pressure_psi.is_none());assert_eq!(view.valves[0].readback_age_ms,None);assert_eq!(view.valves[0].fresh_readback_open,None);
        input.pressure=Some(Sample{value:0.,at_ms:1000});input.valves[0]=Some(ValveState{open:false,at_ms:1000});
        let view=engine.progress_observation(input);assert_eq!(view.fresh_pressure_psi,Some(0.));assert_eq!(view.valves[0].fresh_readback_open,Some(false));
    }

}

#[cfg(test)]
mod operator_coherence_tests;

#[cfg(test)]
mod pause_resume_tests;

#[cfg(test)]
mod failure_pause_tests;
