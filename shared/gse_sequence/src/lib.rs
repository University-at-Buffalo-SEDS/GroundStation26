//! Deterministic ground-fill sequencing. This crate performs no hardware I/O.
use serde::{Deserialize, Serialize};

/// Order is pilot, vent, dump, nitrogen, nitrous. Ignition/retraction are never tested.
pub const RELIEVED: [bool; 5] = [false, true, true, false, false];
pub const CLOSED: [bool; 5] = [false; 5];
pub const FILL_READY: [bool; 5] = [false, true, false, false, false];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub nitrogen_target_psi: f32,
    pub pressure_step_psi: f32,
    pub pressure_ceiling_psi: Option<f32>,
    pub maximum_zero_offset_psi: Option<f32>,
    pub dry_self_test_confirmed: bool,
    pub grouped_panel: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            nitrogen_target_psi: 120.0,
            pressure_step_psi: 50.0,
            pressure_ceiling_psi: None,
            maximum_zero_offset_psi: None,
            dry_self_test_confirmed: false,
            grouped_panel: true,
        }
    }
}
impl Config {
    /// Incomplete limits may be saved, but cannot enable automatic operations.
    pub fn validate_settings(&self) -> Result<(), String> {
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
        if self.pressure_ceiling_psi.is_some() && self.maximum_zero_offset_psi.is_some() {
            self.validate()?;
        }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        let ceiling = self
            .pressure_ceiling_psi
            .ok_or("Set a pressure ceiling before running automatic GSE actions")?;
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
            || self.nitrogen_target_psi + zero >= ceiling
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Action {
    StartFill,
    PauseFill,
    CancelFill,
    SelfTest,
    NitrogenTest,
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
}
#[derive(Clone, Debug, Serialize)]
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
    pub status: Status,
    pub claimed: bool,
    phase_since: u64,
    target: [bool; 5],
    samples: Vec<Sample>,
    self_index: usize,
    testing_valves: bool,
    hold_reference: Option<f32>,
    hold_samples: usize,
}
impl Default for Engine {
    fn default() -> Self {
        Self {
            config: Config::default(),
            status: Status {
                phase: Phase::Idle,
                message: "Ready for GSE checkout".into(),
                nitrogen_passed: false,
                self_test_locked: false,
                current_step_psi: 0.0,
                baseline: None,
                pressure_psi: None,
                valves: [None; 5],
            },
            claimed: false,
            phase_since: 0,
            target: CLOSED,
            samples: Vec::new(),
            self_index: 0,
            testing_valves: false,
            hold_reference: None,
            hold_samples: 0,
        }
    }
}
impl Engine {
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
            );
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
                self.status.nitrogen_passed
                    && matches!(self.status.phase, Phase::Passed | Phase::Paused)
                    && self
                        .status
                        .baseline
                        .as_ref()
                        .is_some_and(|b| b.is_zero(Self::fresh_pressure(input).unwrap()))
            }
            _ => false,
        }
    }
    fn transition(&mut self, phase: Phase, now: u64, message: &str) {
        self.status.phase = phase;
        self.phase_since = now;
        self.status.message = message.into();
    }
    fn configure(&mut self, phase: Phase, desired: [bool; 5], now: u64, message: &str) -> Effects {
        self.target = desired;
        self.transition(phase, now, message);
        // Close supply valves before any other reconfiguration; opening a supply
        // happens only in a subsequent phase after all prerequisites are acknowledged.
        Effects {
            valves: [3, 4, 0, 1, 2]
                .into_iter()
                .map(|i| (i, desired[i]))
                .collect(),
            notification: None,
        }
    }
    pub fn request(&mut self, action: Action, input: Inputs) -> Result<Effects, String> {
        if !self.allows(action, input) {
            return Err(
                "GSE action blocked: check limits, fresh PT, sequence state and interlocks".into(),
            );
        }
        self.claimed = true;
        Ok(match action {
            Action::CancelFill => {
                self.status.nitrogen_passed = false;
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
                self.status.self_test_locked = true;
                self.configure(
                    Phase::FillSetup,
                    FILL_READY,
                    input.now_ms,
                    "Configuring for nitrous fill",
                )
            }
            Action::SelfTest | Action::NitrogenTest => {
                self.status.nitrogen_passed = false;
                self.status.baseline = None;
                self.testing_valves = action == Action::SelfTest;
                self.config.dry_self_test_confirmed = false;
                if !self.testing_valves {
                    self.status.self_test_locked = true;
                }
                self.samples.clear();
                self.configure(
                    Phase::BaselineSetup,
                    RELIEVED,
                    input.now_ms,
                    "Preparing empty-tank pressure calibration",
                )
            }
        })
    }
    pub fn fault(&mut self, now: u64, message: &str) -> Effects {
        self.status.nitrogen_passed = false;
        let mut effects = self.configure(Phase::Fault, RELIEVED, now, message);
        effects.notification = Some(format!(
            "GSE sequence failed: {message}. Supply close and dump/vent open requested; verify tank pressure."
        ));
        effects
    }
    pub fn manual_override(&mut self) {
        self.status.nitrogen_passed = false;
    }
    fn confirmed(&self, input: Inputs) -> bool {
        input
            .valves
            .iter()
            .zip(self.target)
            .all(|(v, target)| v.is_some_and(|v| v.open == target && v.at_ms >= self.phase_since))
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
        let elapsed = input.now_ms.saturating_sub(self.phase_since);
        if matches!(
            self.status.phase,
            Phase::CancelSetup | Phase::PauseSetup | Phase::SelfFinish
        ) {
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
                    } else if phase == Phase::Paused {
                        "Fill paused: all valves closed"
                    } else {
                        "Fill cancelled: dump and vent open"
                    },
                );
                return Effects {
                    notification: Some(self.status.message.clone()),
                    ..Effects::default()
                };
            }
            if elapsed > 5000 {
                return self.fault(input.now_ms, "Valve acknowledgement timed out");
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
            return self.fault(input.now_ms, "Tank pressure is missing, invalid or stale");
        };
        if self
            .config
            .pressure_ceiling_psi
            .is_none_or(|limit| pressure >= limit)
        {
            return self.fault(input.now_ms, "Pressure ceiling reached");
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
        if setup && elapsed > 5000 {
            return self.fault(input.now_ms, "Valve acknowledgement timed out");
        }
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
                    return self.fault(
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
                self.status.current_step_psi = self
                    .config
                    .pressure_step_psi
                    .min(self.config.nitrogen_target_psi);
                return self.configure(
                    if self.testing_valves {
                        Phase::SelfSetup
                    } else {
                        Phase::NitrogenSetup
                    },
                    CLOSED,
                    input.now_ms,
                    "Closing all valves before testing",
                );
            }
            Phase::SelfSetup if self.confirmed(input) => {
                let mut target = CLOSED;
                target[self.self_index] = true;
                return self.configure(
                    Phase::SelfOpen,
                    target,
                    input.now_ms,
                    "Opening one valve for self-test",
                );
            }
            Phase::SelfOpen if self.confirmed(input) => self.transition(
                Phase::SelfHold,
                input.now_ms,
                "Valve self-test: two-second dwell",
            ),
            Phase::SelfHold if elapsed >= 2000 => {
                return self.configure(
                    Phase::SelfClose,
                    CLOSED,
                    input.now_ms,
                    "Closing tested valve",
                );
            }
            Phase::SelfClose if self.confirmed(input) => {
                self.self_index += 1;
                if self.self_index == 5 {
                    return self.configure(
                        Phase::SelfFinish,
                        RELIEVED,
                        input.now_ms,
                        "Restoring dump and vent to open",
                    );
                }
                let mut target = CLOSED;
                target[self.self_index] = true;
                return self.configure(
                    Phase::SelfOpen,
                    target,
                    input.now_ms,
                    "Opening next valve for self-test",
                );
            }
            Phase::NitrogenSetup if self.confirmed(input) => {
                let mut target = CLOSED;
                target[3] = true;
                return self.configure(
                    Phase::Raising,
                    target,
                    input.now_ms,
                    "Raising nitrogen pressure to the next 50 psi step",
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
                    return self.fault(input.now_ms, "Nitrogen pressure step timed out");
                }
            }
            Phase::Settling => {
                if elapsed > 5000 && !self.confirmed(input) {
                    return self.fault(input.now_ms, "Nitrogen close acknowledgement timed out");
                }
                if elapsed >= 2000
                    && self.confirmed(input)
                    && let Some(mean) = self.sample_mean(input.now_ms.saturating_sub(1000))
                {
                    let baseline = self.status.baseline.as_ref().unwrap();
                    if mean
                        < baseline.average_psi + self.status.current_step_psi
                            - 3.0
                            - baseline.noise_psi
                    {
                        return self.fault(
                            input.now_ms,
                            "Pressure fell below the test step while settling",
                        );
                    }
                    self.hold_reference = Some(mean);
                    self.hold_samples = 0;
                    self.transition(
                        Phase::Holding,
                        input.now_ms,
                        "Checking for pressure loss over 5 seconds",
                    );
                }
            }
            Phase::Holding => {
                if new_sample.is_some() {
                    self.hold_samples += 1;
                }
                if let Some(mean) = self.sample_mean(input.now_ms.saturating_sub(1000)) {
                    let noise = self.status.baseline.as_ref().unwrap();
                    if self.hold_reference.unwrap() - mean > 3.0 + 2.0 * noise.noise_psi {
                        return self.fault(
                            input.now_ms,
                            "Pressure drop exceeds 3 psi beyond the measured noise envelope",
                        );
                    }
                }
                if elapsed >= 5000 {
                    if self.hold_samples < 10 {
                        return self.fault(input.now_ms, "Too few samples to verify pressure hold");
                    }
                    let hold_mean = self.sample_mean(self.phase_since).unwrap();
                    if self.hold_reference.unwrap() - hold_mean > 3.0 {
                        return self.fault(input.now_ms, "Sustained mean pressure loss exceeds 3 psi; leak or inconclusive noisy hold, test not passed");
                    }
                    if self.status.current_step_psi >= self.config.nitrogen_target_psi {
                        return self.configure(
                            Phase::Dumping,
                            RELIEVED,
                            input.now_ms,
                            "Nitrogen holds passed; dumping tank before enabling fill",
                        );
                    }
                    self.status.current_step_psi = (self.status.current_step_psi
                        + self.config.pressure_step_psi)
                        .min(self.config.nitrogen_target_psi);
                    return self.configure(
                        Phase::NitrogenSetup,
                        CLOSED,
                        input.now_ms,
                        "Preparing next pressure step",
                    );
                }
            }
            Phase::Dumping => {
                if elapsed > 60000 {
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
                if !self.status.baseline.as_ref().unwrap().is_zero(pressure) {
                    return self.fault(
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
            Phase::Filling if elapsed > 5000 && !self.confirmed(input) => {
                return self.fault(
                    input.now_ms,
                    "Fill valve state does not match the requested configuration",
                );
            }
            _ => {}
        }
        if matches!(
            self.status.phase,
            Phase::Raising | Phase::Holding | Phase::Paused
        ) && input
            .valves
            .iter()
            .zip(self.target)
            .any(|(v, desired)| v.is_none_or(|v| v.open != desired))
            && elapsed > 5000
        {
            return self.fault(
                input.now_ms,
                "Unexpected valve state during automatic sequence",
            );
        }
        Effects::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Rig {
        engine: Engine,
        now: u64,
        pressure: f32,
        valves: [bool; 5],
    }
    impl Rig {
        fn new() -> Self {
            let mut engine = Engine::default();
            engine.config = Config {
                nitrogen_target_psi: 120.0,
                pressure_step_psi: 50.0,
                pressure_ceiling_psi: Some(200.0),
                maximum_zero_offset_psi: Some(8.0),
                dry_self_test_confirmed: true,
                grouped_panel: true,
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
                if self.engine.status.phase == phase {
                    return;
                }
                self.tick();
            }
            panic!("did not reach {phase:?}: {:?}", self.engine.status);
        }
        fn baseline(&mut self) {
            self.request(Action::NitrogenTest);
            self.tick();
            for i in 0..50 {
                self.pressure = if i % 2 == 0 { 3.0 } else { 7.0 };
                self.tick();
            }
            self.pressure = 5.0;
        }
    }
    #[test]
    fn missing_limits_prevent_automatic_actions() {
        assert!(Config::default().validate().is_err());
        let mut rig = Rig::new();
        rig.engine.config.pressure_ceiling_psi = None;
        assert!(!rig.engine.allows(Action::NitrogenTest, rig.input()));
    }
    #[test]
    fn zero_centered_three_psi_noise_is_measured() {
        let mut rig = Rig::new();
        rig.engine.config.maximum_zero_offset_psi = Some(3.0);
        rig.pressure = 0.0;
        rig.request(Action::NitrogenTest);
        rig.tick();
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
        rig.until(Phase::Fault);
        assert!(!rig.engine.status.nitrogen_passed);
        assert_eq!(rig.valves, RELIEVED);
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
        assert_eq!(rig.engine.status.phase, Phase::Dumping);
        assert_eq!(rig.valves, RELIEVED);
        assert!(!rig.engine.status.nitrogen_passed);
        rig.pressure = 5.0;
        rig.until(Phase::Passed);
        assert!(rig.engine.status.nitrogen_passed);
        assert!(!rig.engine.allows(Action::SelfTest, rig.input()));
        rig.request(Action::StartFill);
        assert_eq!(rig.valves, FILL_READY);
        rig.tick();
        assert_eq!(rig.valves, [false, true, false, false, true]);
        rig.request(Action::PauseFill);
        rig.tick();
        assert_eq!(rig.valves, CLOSED);
        rig.request(Action::CancelFill);
        rig.tick();
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
        assert_eq!(rig.engine.status.phase, Phase::Dumping);
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
        rig.until(Phase::Fault);
        assert_eq!(rig.valves, RELIEVED);
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
    fn stale_pressure_faults_instead_of_reporting_pass() {
        let mut rig = Rig::new();
        rig.baseline();
        rig.until(Phase::Raising);
        let mut input = rig.input();
        input.pressure.as_mut().unwrap().at_ms = 0;
        let effects = rig.engine.tick(input);
        rig.effects(effects);
        assert_eq!(rig.engine.status.phase, Phase::Fault);
        assert_eq!(rig.valves, RELIEVED);
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
        rig.effects(effects);
        assert_eq!(rig.engine.status.phase, Phase::Fault);
        assert!(!rig.valves[3] && !rig.valves[4]);
    }
    #[test]
    fn self_test_exercises_one_valve_at_a_time_and_restores_normally_open_valves() {
        let mut rig = Rig::new();
        rig.request(Action::SelfTest);
        rig.until(Phase::SelfHold);
        let mut seen = Vec::new();
        for _ in 0..300 {
            if rig.engine.status.phase == Phase::SelfHold {
                assert_eq!(rig.valves.iter().filter(|v| **v).count(), 1);
                let valve = rig.valves.iter().position(|v| *v).unwrap();
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
}
