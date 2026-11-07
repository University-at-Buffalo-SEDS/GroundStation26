//! Operator-requested, bounded N2 pressurization of already qualified nitrous.
//! No hardware, transport, calibration, authority or live-clock access.
use crate::{CLOSED, Config, Effects, Inputs, Sample, VALVE_ACK_TIMEOUT_MS};
use serde::{Deserialize, Serialize};

fn default_pressure_deadband() -> f32 {
    12.0
}

const RAISE_TIMEOUT_MS: u64 = 60_000; // Same finite raising window as nitrogen testing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub reset_generation: u64,
    pub qualification_generation: u64,
    /// Equality binding only; never cryptographic authority.
    pub configuration: [u8; 32],
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Capture {
    pub value: f32,
    pub at_ms: u64,
}
impl From<Sample> for Capture {
    fn from(s: Sample) -> Self {
        Self {
            value: s.value,
            at_ms: s.at_ms,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub context: Context,
    pub nitrous_target_mass_kg: f32,
    pub pressure_target_psi: f32,
    #[serde(default = "default_pressure_deadband")]
    pub pressure_deadband_psi: f32,
    pub max_added_nitrogen_mass_kg: f32,
    pub before_mass: Capture,
    pub before_pressure: Capture,
    pub reached_pressure: Capture,
    pub after_mass: Capture,
    pub after_pressure: Capture,
    pub added_mass_kg: f32,
    pub completed_at_ms: u64,
    pub mass_samples: u32,
    pub closed_valves_at_ms: [u64; 5],
}
impl Evidence {
    pub fn valid(&self) -> bool {
        [
            self.nitrous_target_mass_kg,
            self.pressure_target_psi,
            self.pressure_deadband_psi,
            self.max_added_nitrogen_mass_kg,
            self.before_mass.value,
            self.after_mass.value,
            self.added_mass_kg,
            self.before_pressure.value,
            self.after_pressure.value,
            self.reached_pressure.value,
        ]
        .into_iter()
        .all(f32::is_finite)
            && self.nitrous_target_mass_kg != 0.0
            && (8.0..=16.0).contains(&self.pressure_deadband_psi)
            && self.pressure_target_psi > self.pressure_deadband_psi
            && self.max_added_nitrogen_mass_kg > 0.0
            && self.mass_samples >= 10
            && self.reached_pressure.value >= self.pressure_target_psi - self.pressure_deadband_psi
            && self.after_pressure.value >= self.pressure_target_psi - self.pressure_deadband_psi
            && self.added_mass_kg <= self.max_added_nitrogen_mass_kg
            && self.completed_at_ms >= self.after_mass.at_ms
            && self
                .closed_valves_at_ms
                .iter()
                .all(|at| *at <= self.completed_at_ms)
    }

    pub fn accepts_mass(&self, context: Context, mass: Sample, now: u64, tolerance: f32) -> bool {
        self.valid()
            && context == self.context
            && fresh(Some(mass), now).is_some()
            && tolerance.is_finite()
            && tolerance > 0.0
            && (mass.value - self.after_mass.value).abs() <= tolerance
            && (mass.value - self.before_mass.value) * self.nitrous_target_mass_kg.signum()
                <= self.max_added_nitrogen_mass_kg
            && (mass.value - self.before_mass.value) * self.nitrous_target_mass_kg.signum()
                >= -tolerance
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Closing,
    OpeningNitrogen,
    Raising,
    ClosingNitrogen,
    Settling,
    Complete,
    Cancelled,
}
/// Copy-only controller state for presentation; outputs remain plans, not motion proof.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProgressObservation {
    pub phase: Phase,
    pub desired: [bool; 5],
    pub pressure_target_psi: f32,
    pub liquid_target_mass_kg: f32,
    pub pending_mask: u8,
    pub waiting: Option<usize>,
    pub command_since_ms: u64,
    pub phase_since_ms: u64,
    pub settle_sample_count: u32,
}
#[derive(Default, Debug)]
pub struct Update {
    pub effects: Effects,
    pub severe_pressure: bool,
    pub completed: Option<Evidence>,
    pub cancelled: Option<String>,
}
pub struct Controller {
    phase: Phase,
    context: Context,
    target_mass: f32,
    pressure_target: f32,
    pressure_deadband: f32,
    pressure_limit: f32,
    max_added: f32,
    tolerance: f32,
    settle_ms: u64,
    before_mass: Sample,
    before_pressure: Sample,
    reached_pressure: Option<Sample>,
    desired: [bool; 5],
    pending: u8,
    waiting: Option<usize>,
    command_since: u64,
    since: u64,
    last_now: u64,
    first_settle_at: Option<u64>,
    last_settle_at: Option<u64>,
    count: u32,
    sum: f64,
    low: f32,
    high: f32,
}
fn fresh(sample: Option<Sample>, now: u64) -> Option<Sample> {
    sample.filter(|s| s.value.is_finite() && s.at_ms <= now && now - s.at_ms <= 2000)
}
impl Controller {
    pub fn start(
        config: &Config,
        target_mass: f32,
        pressure_target: f32,
        input: Inputs,
        mass: Option<Sample>,
        context: Context,
    ) -> Result<(Self, Effects), String> {
        Self::start_with_inventory(
            config,
            target_mass,
            pressure_target,
            input,
            mass,
            context,
            None,
        )
    }
    /// Repeated top-ups retain the original qualified nitrous inventory anchor.
    pub fn start_with_inventory(
        config: &Config,
        target_mass: f32,
        pressure_target: f32,
        input: Inputs,
        mass: Option<Sample>,
        context: Context,
        prior: Option<&Evidence>,
    ) -> Result<(Self, Effects), String> {
        config.validate_fill_trim()?;
        config.validate_turbocharge_deadband()?;
        let max_added = config
            .turbocharge_max_added_mass_kg
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or("Configure a maximum added nitrogen mass (kg) before Turbocharge")?;
        let ceiling = config
            .pressure_ceiling_psi
            .filter(|v| v.is_finite() && *v > 0.0)
            .ok_or("Configure the hardware pressure ceiling before Turbocharge")?;
        if !target_mass.is_finite()
            || target_mass == 0.0
            || !pressure_target.is_finite()
            || pressure_target <= config.turbocharge_pressure_deadband_psi
            || pressure_target >= ceiling
        {
            return Err("Turbocharge requires a valid nitrous mass target and pressure target below the hardware ceiling".into());
        }
        if !input.prelaunch || !input.interlock {
            return Err("Turbocharge requires prelaunch state and the operating interlock".into());
        }
        let mass = fresh(mass, input.now_ms).ok_or("Fresh calibrated fill mass is required")?;
        if prior.is_none()
            && !crate::fill_resume::inventory_ready(
                mass.value,
                target_mass,
                config.fill_tolerance_kg,
            )
        {
            return Err("Nitrous inventory must first be within its qualified mass band".into());
        }
        if prior.is_some_and(|e| {
            e.nitrous_target_mass_kg != target_mass
                || e.pressure_target_psi != pressure_target
                || e.pressure_deadband_psi != config.turbocharge_pressure_deadband_psi
                || e.max_added_nitrogen_mass_kg != max_added
                || !e.accepts_mass(context, mass, input.now_ms, config.fill_tolerance_kg)
        }) {
            return Err("Previous Turbocharge inventory evidence no longer matches".into());
        }
        let inventory = prior.map_or(mass, |e| Sample {
            value: e.before_mass.value,
            at_ms: e.before_mass.at_ms,
        });
        let pressure = fresh(input.pressure, input.now_ms)
            .filter(|p| p.value >= 0.0)
            .ok_or("Fresh valve-board pressure (psi) is required")?;
        let pressure_limit = config.pressure_limit(pressure_target, 50.0)?;
        if pressure.value >= pressure_limit {
            return Err("Tank pressure is at or above the configured limit".into());
        }
        let mut c = Self {
            phase: Phase::Closing,
            context,
            target_mass,
            pressure_target,
            pressure_deadband: config.turbocharge_pressure_deadband_psi,
            pressure_limit,
            max_added,
            tolerance: config.fill_tolerance_kg,
            settle_ms: config.fill_settle_ms,
            before_mass: inventory,
            before_pressure: pressure,
            reached_pressure: None,
            desired: CLOSED,
            pending: 31,
            waiting: None,
            command_since: input.now_ms,
            since: input.now_ms,
            last_now: input.now_ms,
            first_settle_at: None,
            last_settle_at: None,
            count: 0,
            sum: 0.0,
            low: f32::INFINITY,
            high: f32::NEG_INFINITY,
        };
        let effects = c.next(input.now_ms);
        Ok((c, effects))
    }
    pub fn inventory_anchor(&self) -> Sample {
        self.before_mass
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn progress_observation(&self) -> ProgressObservation {
        ProgressObservation {
            phase: self.phase,
            desired: self.desired,
            pressure_target_psi: self.pressure_target,
            liquid_target_mass_kg: self.target_mass,
            pending_mask: self.pending,
            waiting: self.waiting,
            command_since_ms: self.command_since,
            phase_since_ms: self.since,
            settle_sample_count: self.count,
        }
    }
    fn next(&mut self, now: u64) -> Effects {
        let order = [3, 4, 0, 1, 2];
        self.waiting = order
            .into_iter()
            .find(|i| self.pending & (1 << i) != 0 && !self.desired[*i])
            .or_else(|| order.into_iter().find(|i| self.pending & (1 << i) != 0));
        self.command_since = now;
        if let Some(i) = self.waiting {
            self.pending &= !(1 << i);
            Effects {
                valves: vec![(i, self.desired[i])],
                notification: None,
            }
        } else {
            Effects::default()
        }
    }
    fn configure(&mut self, phase: Phase, desired: [bool; 5], now: u64) -> Update {
        self.pending = desired.iter().enumerate().fold(0, |mask, (i, v)| {
            mask | if *v != self.desired[i] { 1 << i } else { 0 }
        });
        self.phase = phase;
        self.desired = desired;
        self.since = now;
        self.waiting = None;
        Update {
            effects: self.next(now),
            ..Update::default()
        }
    }
    fn cancel(&mut self, message: &str, may_close: bool) -> Update {
        self.phase = Phase::Cancelled;
        self.pending = 0;
        self.waiting = None;
        Update {
            effects: Effects {
                valves: if may_close {
                    [3, 4, 0, 1, 2].into_iter().map(|i| (i, false)).collect()
                } else {
                    vec![]
                },
                notification: Some(message.into()),
            },
            cancelled: Some(message.into()),
            completed: None,
            severe_pressure: false,
        }
    }
    pub fn tick(&mut self, input: Inputs, mass: Option<Sample>, context: Context) -> Update {
        if matches!(self.phase, Phase::Cancelled | Phase::Complete) {
            return Update::default();
        }
        if context.reset_generation != self.context.reset_generation || !input.prelaunch {
            return self.cancel("Turbocharge invalidated by launch/recovery generation; no automatic outputs issued",false);
        }
        if context != self.context {
            return self.cancel(
                "Turbocharge source, route, calibration or qualification binding changed; pausing",
                true,
            );
        }
        if input.now_ms < self.last_now {
            return self.cancel("Turbocharge clock regressed; pausing", true);
        }
        self.last_now = input.now_ms;
        let Some(pressure) = fresh(input.pressure, input.now_ms).filter(|p| p.value >= 0.0) else {
            return self.cancel(
                "Valve-board pressure is missing, invalid or stale; pausing",
                true,
            );
        };
        if pressure.value >= self.pressure_limit {
            let mut update = self.cancel("Turbocharge pressure ceiling reached", true);
            update.severe_pressure = true;
            return update;
        }
        if !input.interlock {
            return self.cancel("Turbocharge interlock lost; pausing", true);
        }
        let Some(mass) = fresh(mass, input.now_ms) else {
            return self.cancel("Turbocharge mass missing, invalid or stale; pausing", true);
        };
        let delta = (mass.value - self.before_mass.value) * self.target_mass.signum();
        if delta > self.max_added || delta < -self.tolerance {
            return self.cancel(
                "Turbocharge mass is outside the explicit added-mass/loss bounds; pausing",
                true,
            );
        }
        if let Some(i) = self.waiting {
            match input.valves[i].filter(|v| {
                v.at_ms > self.command_since
                    && v.at_ms <= input.now_ms
                    && input.now_ms - v.at_ms <= 2000
            }) {
                Some(v) if v.open == self.desired[i] => {
                    self.waiting = None;
                    let effects = self.next(input.now_ms);
                    if self.waiting.is_some() {
                        return Update {
                            effects,
                            ..Update::default()
                        };
                    }
                }
                Some(_) => {
                    return self.cancel(
                        "Turbocharge valve reported the opposite state; pausing",
                        true,
                    );
                }
                None if input.now_ms.saturating_sub(self.command_since) >= VALVE_ACK_TIMEOUT_MS => {
                    return self.cancel("Turbocharge valve confirmation timed out; pausing", true);
                }
                None => return Update::default(),
            }
        }
        if self.waiting.is_some() {
            return Update::default();
        }
        if input.valves.iter().enumerate().any(|(i, v)| {
            v.is_none_or(|v| {
                v.open != self.desired[i] || v.at_ms > input.now_ms || input.now_ms - v.at_ms > 2000
            })
        }) {
            return self.cancel(
                "Turbocharge valve state is missing, stale or mismatched; pausing",
                true,
            );
        }
        match self.phase {
            Phase::Closing => {
                if pressure.value >= self.pressure_target - self.pressure_deadband {
                    self.reached_pressure = Some(pressure);
                    self.phase = Phase::Settling;
                    self.since = input.now_ms;
                } else {
                    let mut desired = CLOSED;
                    desired[3] = true;
                    return self.configure(Phase::OpeningNitrogen, desired, input.now_ms);
                }
            }
            Phase::OpeningNitrogen => {
                self.phase = Phase::Raising;
                self.since = input.now_ms;
                if pressure.value >= self.pressure_target {
                    self.reached_pressure = Some(pressure);
                    return self.configure(Phase::ClosingNitrogen, CLOSED, input.now_ms);
                }
            }
            Phase::Raising => {
                if pressure.value >= self.pressure_target {
                    self.reached_pressure = Some(pressure);
                    return self.configure(Phase::ClosingNitrogen, CLOSED, input.now_ms);
                }
                if input.now_ms.saturating_sub(self.since) >= RAISE_TIMEOUT_MS {
                    return self.cancel("Turbocharge pressure target timed out; pausing", true);
                }
            }
            Phase::ClosingNitrogen => {
                self.phase = Phase::Settling;
                self.since = input.now_ms;
            }
            Phase::Settling => {
                if pressure.value < self.pressure_target - self.pressure_deadband {
                    return self.cancel(
                        "Pressure fell below the Turbocharge deadband while settling; paused for operator review",
                        true,
                    );
                }
                if mass.at_ms >= self.since && self.last_settle_at.is_none_or(|t| mass.at_ms > t) {
                    self.first_settle_at.get_or_insert(mass.at_ms);
                    self.last_settle_at = Some(mass.at_ms);
                    if self.count < u32::MAX {
                        self.count += 1;
                        self.sum += mass.value as f64;
                        self.low = self.low.min(mass.value);
                        self.high = self.high.max(mass.value);
                    }
                }
                if input.now_ms.saturating_sub(self.since) >= self.settle_ms
                    && self.count >= 10
                    && self
                        .first_settle_at
                        .zip(self.last_settle_at)
                        .is_some_and(|(a, b)| b.saturating_sub(a) >= self.settle_ms)
                    && self.high - self.low <= 2.0 * self.tolerance
                {
                    let mean = (self.sum / self.count as f64) as f32;
                    let added = (mean - self.before_mass.value) * self.target_mass.signum();
                    if !mean.is_finite() || added > self.max_added || added < -self.tolerance {
                        return self.cancel(
                            "Settled Turbocharge mass is outside its bounds; pausing",
                            true,
                        );
                    }
                    let Some(reached_pressure) = self.reached_pressure else {
                        return self.cancel(
                            "Turbocharge pressure target has no recorded evidence; pausing",
                            true,
                        );
                    };
                    self.phase = Phase::Complete;
                    return Update {
                        severe_pressure: false,
                        completed: Some(Evidence {
                            context: self.context,
                            nitrous_target_mass_kg: self.target_mass,
                            pressure_target_psi: self.pressure_target,
                            pressure_deadband_psi: self.pressure_deadband,
                            max_added_nitrogen_mass_kg: self.max_added,
                            before_mass: self.before_mass.into(),
                            before_pressure: self.before_pressure.into(),
                            reached_pressure: reached_pressure.into(),
                            after_mass: Capture {
                                value: mean,
                                at_ms: mass.at_ms,
                            },
                            after_pressure: pressure.into(),
                            added_mass_kg: added,
                            completed_at_ms: input.now_ms,
                            mass_samples: self.count,
                            closed_valves_at_ms: input
                                .valves
                                .map(|v| v.expect("confirmed closed valves").at_ms),
                        }),
                        ..Update::default()
                    };
                }
                if input.now_ms.saturating_sub(self.since) > self.settle_ms + VALVE_ACK_TIMEOUT_MS {
                    return self.cancel("Turbocharge mass did not settle; pausing", true);
                }
            }
            Phase::Complete | Phase::Cancelled => {}
        }
        Update::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValveState;
    fn config() -> Config {
        Config {
            pressure_ceiling_psi: Some(900.0),
            turbocharge_max_added_mass_kg: Some(0.3),
            fill_settle_ms: 1000,
            ..Config::default()
        }
    }
    fn context() -> Context {
        Context {
            reset_generation: 1,
            qualification_generation: 2,
            configuration: [3; 32],
        }
    }
    #[test]
    fn progress_getter_reads_actual_targets_without_advancing_controller() {
        let rig = Rig::new(1.0);
        let before = (
            rig.c.phase,
            rig.c.desired,
            rig.c.pending,
            rig.c.waiting,
            rig.c.command_since,
            rig.c.since,
            rig.c.count,
            rig.c.sum.to_bits(),
        );
        for _ in 0..5 {
            let view = rig.c.progress_observation();
            assert_eq!(view.phase, rig.c.phase);
            assert_eq!(view.desired, rig.c.desired);
            assert_eq!(
                view.pressure_target_psi.to_bits(),
                rig.c.pressure_target.to_bits()
            );
            assert_eq!(
                view.liquid_target_mass_kg.to_bits(),
                rig.c.target_mass.to_bits()
            );
        }
        assert_eq!(
            before,
            (
                rig.c.phase,
                rig.c.desired,
                rig.c.pending,
                rig.c.waiting,
                rig.c.command_since,
                rig.c.since,
                rig.c.count,
                rig.c.sum.to_bits()
            )
        );
    }
    #[test]
    fn pressure_deadband_boundaries_settle_closed_and_never_restart() {
        for band in [8.0_f32, 12.0, 16.0] {
            for pressure in [745.0, 745.0 - band, (745.0 - band).next_up(), 745.0 - band / 2.0] {
                let mut r = Rig::new(10.0);
                r.c.pressure_deadband = band;
                r.pressure = pressure;
                for _ in 0..60 {
                    r.step();
                    if r.evidence.is_some() { break; }
                }
                let evidence = r.evidence.as_ref().expect("in-band closed tank settles");
                assert!(evidence.valid());
                assert_eq!(evidence.pressure_deadband_psi, band);
                assert!(!r.events.contains(&(3, true)));
                let count = r.events.len();
                r.pressure = 100.0;
                for _ in 0..10 { r.step(); }
                assert_eq!(r.c.phase(), Phase::Complete);
                assert_eq!(r.events.len(), count, "completed controller never restarts");
            }
            let mut below = Rig::new(10.0);
            below.c.pressure_deadband = band;
            below.pressure = (745.0 - band).next_down();
            below.reach_raising();
            assert!(below.events.contains(&(3, true)));
            // Hysteresis does not move the nitrogen shutoff point below target.
            below.pressure = 744.875;
            assert!(below.step().effects.valves.is_empty());
            assert_eq!(below.c.phase(), Phase::Raising);
            below.pressure = 745.0;
            assert_eq!(below.step().effects.valves, vec![(3, false)]);
            while below.c.phase() != Phase::Settling { below.step(); }
            for index in 0..40 {
                below.pressure = if index % 2 == 0 { 745.0 - band } else { 745.0 };
                let update = below.step();
                assert!(update.cancelled.is_none());
                if below.evidence.is_some() { break; }
            }
            assert!(below.evidence.as_ref().is_some_and(Evidence::valid));
            let mut low = Rig::new(10.0);
            low.c.pressure_deadband = band;
            low.pressure = 745.0;
            while low.c.phase() != Phase::Settling { low.step(); }
            low.pressure = (745.0 - band).next_down();
            assert!(low.step().cancelled.is_some());
            assert_eq!(low.c.phase(), Phase::Cancelled);
            assert!(low.c.desired.iter().all(|open| !open));
        }
    }

    #[test]
    fn deadband_defaults_validate_and_cannot_make_zero_pressure_ready() {
        let old: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(old.turbocharge_pressure_deadband_psi, 12.0);
        for band in [f32::NAN, f32::INFINITY, 0.0, 7.99, 16.01] {
            let mut cfg = config();
            cfg.turbocharge_pressure_deadband_psi = band;
            assert!(cfg.validate_settings().is_err());
            let r = Rig::new(10.0);
            assert!(Controller::start(&cfg, 10., 745., r.input(), Some(Sample {
                value: 10., at_ms: r.now,
            }), context()).is_err());
        }
        for band in [8., 12., 16.] {
            let mut cfg = config();
            cfg.turbocharge_pressure_deadband_psi = band;
            let r = Rig::new(10.0);
            assert!(Controller::start(&cfg, 10., band, r.input(), Some(Sample {
                value: 10., at_ms: r.now,
            }), context()).is_err());
        }
    }

    #[test]
    fn outstanding_command_requires_strictly_newer_observation() {
        let mut rig = Rig::new(1.0);
        let i = rig.c.waiting.unwrap();
        rig.valves[i] = Some(ValveState {
            open: rig.c.desired[i],
            at_ms: rig.now,
        });
        let old = rig.c.tick(
            rig.input(),
            Some(Sample {
                value: rig.mass,
                at_ms: rig.now,
            }),
            context(),
        );
        assert!(old.effects.valves.is_empty());
        assert_eq!(rig.c.waiting, Some(i));
        rig.now += 1;
        rig.valves[i].as_mut().unwrap().at_ms = rig.now;
        let fresh = rig.c.tick(
            rig.input(),
            Some(Sample {
                value: rig.mass,
                at_ms: rig.now,
            }),
            context(),
        );
        assert_eq!(fresh.effects.valves.len(), 1);
        assert_ne!(rig.c.waiting, Some(i));
    }
    struct Rig {
        c: Controller,
        now: u64,
        mass: f32,
        pressure: f32,
        valves: [Option<ValveState>; 5],
        events: Vec<(usize, bool)>,
        evidence: Option<Evidence>,
    }
    impl Rig {
        fn new(target: f32) -> Self {
            Self::with_config(target, &config())
        }
        fn with_config(target: f32, config: &Config) -> Self {
            let input = Inputs {
                now_ms: 100,
                pressure: Some(Sample {
                    value: 100.0,
                    at_ms: 100,
                }),
                valves: [None; 5],
                interlock: true,
                prelaunch: true,
            };
            let (c, e) = Controller::start(
                config,
                target,
                745.0,
                input,
                Some(Sample {
                    value: target,
                    at_ms: 100,
                }),
                context(),
            )
            .unwrap();
            let mut r = Self {
                c,
                now: 100,
                mass: target,
                pressure: 100.0,
                valves: [None; 5],
                events: vec![],
                evidence: None,
            };
            r.apply(e);
            r
        }
        fn apply(&mut self, e: Effects) {
            for (i, open) in e.valves {
                assert_ne!((i, open), (4, true), "Turbocharge never admits nitrous");
                self.events.push((i, open));
                self.valves[i] = Some(ValveState {
                    open,
                    at_ms: self.now + 1,
                });
            }
        }
        fn input(&self) -> Inputs {
            Inputs {
                now_ms: self.now,
                pressure: Some(Sample {
                    value: self.pressure,
                    at_ms: self.now,
                }),
                valves: self.valves,
                interlock: true,
                prelaunch: true,
            }
        }
        fn step(&mut self) -> Update {
            self.now += 100;
            for state in self.valves.iter_mut().flatten() {
                state.at_ms = self.now;
            } // explicit fresh inert physical reports
            let u = self.c.tick(
                self.input(),
                Some(Sample {
                    value: self.mass,
                    at_ms: self.now,
                }),
                context(),
            );
            self.apply(Effects {
                valves: u.effects.valves.clone(),
                notification: None,
            });
            if let Some(e) = &u.completed {
                self.evidence = Some(e.clone());
            }
            u
        }
        fn reach_raising(&mut self) {
            for _ in 0..10 {
                self.step();
                if self.c.phase() == Phase::Raising {
                    return;
                }
            }
            panic!("raising not reached");
        }
    }
    #[test]
    fn closes_every_valve_before_n2_and_records_bounded_added_mass_outside_old_band() {
        let mut r = Rig::new(10.0);
        assert_eq!(r.events, vec![(3, false)]);
        r.reach_raising();
        assert_eq!(
            &r.events[..6],
            &[
                (3, false),
                (4, false),
                (0, false),
                (1, false),
                (2, false),
                (3, true)
            ]
        );
        r.mass = 10.2;
        r.pressure = 745.0;
        r.step();
        assert!(r.events.contains(&(3, false)));
        for _ in 0..20 {
            r.step();
            if r.evidence.is_some() {
                break;
            }
        }
        let e = r.evidence.unwrap();
        assert_eq!(e.nitrous_target_mass_kg, 10.0);
        assert!((e.added_mass_kg - 0.2).abs() < 0.0001);
        assert!((e.after_mass.value - e.nitrous_target_mass_kg).abs() > config().fill_tolerance_kg);
        assert!(e.accepts_mass(
            context(),
            Sample {
                value: 10.2,
                at_ms: r.now
            },
            r.now,
            config().fill_tolerance_kg
        ));
        assert!(!e.accepts_mass(
            context(),
            Sample {
                value: 10.31,
                at_ms: r.now
            },
            r.now,
            config().fill_tolerance_kg
        ));
        assert!(r.valves.iter().all(|v| v.is_some_and(|v| !v.open)));
        let json = serde_json::to_string(&e).unwrap();
        let restored: Evidence = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context, e.context);
    }
    #[test]
    fn unset_limits_invalid_targets_and_unqualified_inventory_refuse_start() {
        let r = Rig::new(10.0);
        let i = r.input();
        let m = Some(Sample {
            value: 10.0,
            at_ms: r.now,
        });
        let mut c = config();
        c.turbocharge_max_added_mass_kg = None;
        assert!(Controller::start(&c, 10.0, 745.0, i, m, context()).is_err());
        c = config();
        c.pressure_ceiling_psi = None;
        assert!(Controller::start(&c, 10.0, 745.0, i, m, context()).is_err());
        assert!(Controller::start(&config(), 10.0, 900.0, i, m, context()).is_err());
        assert!(
            Controller::start(
                &config(),
                10.0,
                745.0,
                i,
                Some(Sample {
                    value: 9.0,
                    at_ms: r.now
                }),
                context()
            )
            .is_err()
        );
    }
    #[test]
    fn reset_or_launch_never_emits_cancel_outputs_from_old_controller() {
        for reset in [true, false] {
            let mut r = Rig::new(10.0);
            let mut i = r.input();
            let mut ctx = context();
            if reset {
                ctx.reset_generation += 1;
            } else {
                i.prelaunch = false;
            }
            let u = r.c.tick(
                i,
                Some(Sample {
                    value: 10.0,
                    at_ms: r.now,
                }),
                ctx,
            );
            assert!(u.cancelled.is_some() && u.effects.valves.is_empty());
        }
    }
    #[test]
    fn missing_stale_mass_pressure_interlock_and_overfill_pause_without_abort() {
        for case in 0..6 {
            let mut r = Rig::new(10.0);
            r.reach_raising();
            let mut i = r.input();
            let mut m = Some(Sample {
                value: r.mass,
                at_ms: r.now,
            });
            match case {
                0 => m = None,
                1 => {
                    i.now_ms += 2001;
                }
                2 => i.pressure = None,
                3 => i.interlock = false,
                4 => {
                    m = Some(Sample {
                        value: 10.31,
                        at_ms: r.now,
                    })
                }
                _ => {
                    m = Some(Sample {
                        value: 9.8,
                        at_ms: r.now,
                    })
                }
            }
            let u = r.c.tick(i, m, context());
            assert!(u.cancelled.is_some());
            assert_eq!(u.effects.valves.len(), 5);
            assert!(u.effects.valves.iter().all(|(_, on)| !*on));
        }
    }
    #[test]
    fn opposite_or_missing_ack_never_opens_nitrogen() {
        let mut r = Rig::new(10.0);
        r.now += 1;
        r.valves[3] = Some(ValveState {
            open: true,
            at_ms: r.now,
        });
        let u = r.c.tick(
            r.input(),
            Some(Sample {
                value: r.mass,
                at_ms: r.now,
            }),
            context(),
        );
        assert!(u.cancelled.is_some());
        assert!(!r.events.contains(&(3, true)));
        let mut r = Rig::new(10.0);
        r.now += VALVE_ACK_TIMEOUT_MS;
        r.valves = [None; 5];
        let u = r.c.tick(
            r.input(),
            Some(Sample {
                value: r.mass,
                at_ms: r.now,
            }),
            context(),
        );
        assert!(u.cancelled.is_some());
    }
    #[test]
    fn accepted_open_report_already_at_target_closes_n2_in_same_tick() {
        let mut r = Rig::new(10.0);
        while r.c.phase() != Phase::OpeningNitrogen {
            r.step();
        }
        r.pressure = 745.0;
        let u = r.step();
        assert_eq!(u.effects.valves, vec![(3, false)]);
    }
    #[test]
    fn negative_calibrated_mass_direction_uses_bounded_added_inventory() {
        let mut r = Rig::new(-10.0);
        r.reach_raising();
        r.mass = -10.2;
        r.pressure = 745.0;
        for _ in 0..30 {
            r.step();
            if r.evidence.is_some() {
                break;
            }
        }
        let e = r.evidence.unwrap();
        assert!(e.added_mass_kg > 0.19 && e.added_mass_kg < 0.21);
    }
    #[test]
    fn no_repeated_capture_can_manufacture_settled_evidence() {
        let mut r = Rig::new(10.0);
        r.reach_raising();
        r.pressure = 745.0;
        while r.c.phase() != Phase::Settling {
            r.step();
        }
        let capture = r.now;
        for _ in 0..30 {
            r.now += 100;
            for v in r.valves.iter_mut().flatten() {
                v.at_ms = r.now;
            }
            let u = r.c.tick(
                r.input(),
                Some(Sample {
                    value: 10.0,
                    at_ms: capture,
                }),
                context(),
            );
            assert!(u.completed.is_none());
        }
        assert_ne!(r.c.phase(), Phase::Complete);
    }
    #[test]
    fn configured_custom_deadband_survives_public_start_close_and_settled_evidence() {
        for band in [8.0_f32, 9.5, 12.0, 16.0] {
            let mut cfg = config();
            cfg.turbocharge_pressure_deadband_psi = band;
            let mut r = Rig::with_config(8.0, &cfg);
            r.reach_raising();
            r.pressure = 745.0;
            assert_eq!(r.step().effects.valves, vec![(3, false)], "exact pressure target closes nitrogen");
            r.pressure = 745.0 - band;
            for _ in 0..40 {
                let update = r.step();
                assert!(update.cancelled.is_none());
                if r.evidence.is_some() { break; }
            }
            let evidence = r.evidence.as_ref().expect("configured boundary settles");
            assert!(evidence.valid());
            assert_eq!(evidence.pressure_deadband_psi.to_bits(), band.to_bits());
            assert_eq!(evidence.after_pressure.value, 745.0 - band);
            assert!(r.valves.iter().all(|v| v.is_some_and(|v| !v.open)));
            assert_eq!(r.events.iter().filter(|&&(i, open)| i == 3 && open).count(), 1);
        }
        for band in [8.0_f32.next_down(), 16.0_f32.next_up(), f32::NEG_INFINITY] {
            let mut cfg = config();cfg.turbocharge_pressure_deadband_psi = band;
            assert!(cfg.validate_settings().is_err());
            let r = Rig::new(8.0);
            assert!(Controller::start(&cfg, 8.0, 745.0, r.input(),
                Some(Sample { value: 8.0, at_ms: r.now }), context()).is_err());
        }
    }

}

#[cfg(test)]
mod cumulative_tests {
    use super::*;
    #[test]
    fn repeated_topup_preserves_original_inventory_and_context() {
        let config = Config {
            turbocharge_max_added_mass_kg: Some(0.3),
            pressure_ceiling_psi: Some(900.0),
            ..Config::default()
        };
        let ctx = Context {
            reset_generation: 1,
            qualification_generation: 2,
            configuration: [3; 32],
        };
        let capture = Capture {
            value: 10.0,
            at_ms: 100,
        };
        let evidence = Evidence {
            context: ctx,
            nitrous_target_mass_kg: 10.0,
            pressure_target_psi: 745.0,
            pressure_deadband_psi: 12.0,
            max_added_nitrogen_mass_kg: 0.3,
            before_mass: capture,
            before_pressure: Capture {
                value: 100.0,
                ..capture
            },
            reached_pressure: Capture {
                value: 745.0,
                ..capture
            },
            after_mass: Capture {
                value: 10.2,
                ..capture
            },
            after_pressure: Capture {
                value: 745.0,
                ..capture
            },
            added_mass_kg: 0.2,
            completed_at_ms: 100,
            mass_samples: 10,
            closed_valves_at_ms: [100; 5],
        };
        let input = Inputs {
            now_ms: 200,
            pressure: Some(Sample {
                value: 700.0,
                at_ms: 200,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        let mass = Some(Sample {
            value: 10.2,
            at_ms: 200,
        });
        let (mut controller, _) = Controller::start_with_inventory(
            &config,
            10.0,
            745.0,
            input,
            mass,
            ctx,
            Some(&evidence),
        )
        .unwrap();
        let update = controller.tick(
            input,
            Some(Sample {
                value: 10.31,
                at_ms: 200,
            }),
            ctx,
        );
        assert!(
            update.cancelled.is_some(),
            "a repeated run cannot allocate a fresh0.3kg allowance"
        );
        let mut changed = ctx;
        changed.reset_generation += 1;
        assert!(
            Controller::start_with_inventory(
                &config,
                10.0,
                745.0,
                input,
                mass,
                changed,
                Some(&evidence)
            )
            .is_err()
        );
        assert!(
            Controller::start_with_inventory(
                &config,
                10.0,
                746.0,
                input,
                mass,
                ctx,
                Some(&evidence)
            )
            .is_err(),
            "context must bind pressure target too"
        );
    }
    #[test]
    fn pressure_ceiling_is_explicit_severe_hazard_but_launch_inhibit_wins() {
        let config = Config {
            turbocharge_max_added_mass_kg: Some(0.3),
            pressure_ceiling_psi: Some(900.0),
            ..Config::default()
        };
        let ctx = Context {
            reset_generation: 1,
            qualification_generation: 0,
            configuration: [0; 32],
        };
        let mass = Some(Sample {
            value: 10.0,
            at_ms: 100,
        });
        let input = Inputs {
            now_ms: 100,
            pressure: Some(Sample {
                value: 100.0,
                at_ms: 100,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        let (mut controller, _) =
            Controller::start(&config, 10.0, 745.0, input, mass, ctx).unwrap();
        let hazardous = Inputs {
            pressure: Some(Sample {
                value: 900.0,
                at_ms: 100,
            }),
            ..input
        };
        assert!(controller.tick(hazardous, mass, ctx).severe_pressure);
        let (mut controller, _) =
            Controller::start(&config, 10.0, 745.0, input, mass, ctx).unwrap();
        assert!(
            controller.tick(hazardous, None, ctx).severe_pressure,
            "missing mass cannot mask fresh overpressure"
        );
        let (mut controller, _) =
            Controller::start(&config, 10.0, 745.0, input, mass, ctx).unwrap();
        let update = controller.tick(
            Inputs {
                prelaunch: false,
                ..hazardous
            },
            mass,
            ctx,
        );
        assert!(!update.severe_pressure);
        assert!(update.effects.valves.is_empty());
    }

}
