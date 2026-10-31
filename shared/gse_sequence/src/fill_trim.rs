//! Closed-loop final mass trimming. No transport, clocks or persistent state.
//! The caller retains topology and hazardous-flow supervision after cancellation.
use crate::{CLOSED, Config, Effects, Inputs, Sample, VALVE_ACK_TIMEOUT_MS};
const PULSE_MS: u64 = 100;
const MAX_PULSES: u8 = 20;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Closing,
    Settling,
    VentOpening,
    Venting,
    VentClosing,
    Complete,
    Fault,
    Cancelled,
}
#[derive(Debug, Default)]
pub struct Update {
    pub effects: Effects,
    pub completed: bool,
    pub fault: Option<String>,
}
pub struct Controller {
    phase: Phase,
    target: f32,
    direction: f32,
    tolerance: f32,
    overfill: f32,
    settle_ms: u64,
    pressure_limit: f32,
    since: u64,
    desired: [bool; 5],
    pending: u8,
    waiting: Option<usize>,
    command_since: u64,
    samples: Vec<Sample>,
    pulses: u8,
}
impl Controller {
    pub fn start(
        now_ms: u64,
        target_mass_kg: f32,
        pressure_limit_psi: f32,
        config: &Config,
    ) -> Result<(Self, Effects), String> {
        config.validate_fill_trim()?;
        if !target_mass_kg.is_finite()
            || target_mass_kg == 0.0
            || !pressure_limit_psi.is_finite()
            || pressure_limit_psi <= 0.0
        {
            return Err("Fill target and pressure limit must be finite and nonzero".into());
        }
        let mut this = Self {
            phase: Phase::Closing,
            target: target_mass_kg,
            direction: target_mass_kg.signum(),
            tolerance: config.fill_tolerance_kg,
            overfill: config.fill_overfill_kg,
            settle_ms: config.fill_settle_ms,
            pressure_limit: pressure_limit_psi,
            since: now_ms,
            desired: CLOSED,
            pending: 0,
            waiting: None,
            command_since: now_ms,
            samples: Vec::new(),
            pulses: 0,
        };
        let effects = this.configure(Phase::Closing, CLOSED, now_ms);
        Ok((this, effects))
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn stop_mass_kg(&self) -> f32 {
        self.target + self.direction * self.overfill
    }
    pub fn cancel(&mut self) {
        self.phase = Phase::Cancelled;
        self.pending = 0;
        self.waiting = None;
        self.samples.clear();
    }
    fn configure(&mut self, phase: Phase, desired: [bool; 5], now: u64) -> Effects {
        // Establish every safe output at handoff. Later pulse transitions only
        // change vent: do not delay its closing behind redundant supply ACKs.
        self.pending = if phase == Phase::Closing {
            31
        } else {
            desired.iter().enumerate().fold(0, |mask, (i, value)| {
                mask | if *value != self.desired[i] { 1 << i } else { 0 }
            })
        };
        self.phase = phase;
        self.desired = desired;
        self.waiting = None;
        self.samples.clear();
        self.next(now)
    }
    fn next(&mut self, now: u64) -> Effects {
        let order = [3, 4, 0, 1, 2];
        let next = order
            .into_iter()
            .find(|i| self.pending & (1 << i) != 0 && !self.desired[*i])
            .or_else(|| order.into_iter().find(|i| self.pending & (1 << i) != 0));
        self.waiting = next;
        self.command_since = now;
        match next {
            Some(i) => {
                self.pending &= !(1 << i);
                Effects {
                    valves: vec![(i, self.desired[i])],
                    notification: None,
                }
            }
            None => Effects::default(),
        }
    }
    fn fail(&mut self, message: &str) -> Update {
        self.phase = Phase::Fault;
        self.pending = 0;
        self.waiting = None;
        // Emergency safety close is deliberately not contingent on an ACK.
        Update {
            effects: Effects {
                valves: [3, 4, 0, 1, 2].into_iter().map(|i| (i, false)).collect(),
                notification: None,
            },
            fault: Some(message.into()),
            completed: false,
        }
    }
    pub fn tick(&mut self, input: Inputs, mass: Option<Sample>) -> Update {
        if matches!(
            self.phase,
            Phase::Complete | Phase::Fault | Phase::Cancelled
        ) {
            return Update::default();
        }
        if !input.interlock || !input.prelaunch {
            return self.fail("Fill trim interlock or prelaunch state lost");
        }
        if !input.pressure.is_some_and(|p| {
            p.value.is_finite()
                && p.at_ms <= input.now_ms
                && input.now_ms - p.at_ms <= 2000
                && p.value < self.pressure_limit
        }) {
            return self.fail("Fill trim pressure is missing, stale or above its limit");
        }
        let Some(mass) = mass.filter(|p| {
            p.value.is_finite() && p.at_ms <= input.now_ms && input.now_ms - p.at_ms <= 2000
        }) else {
            return self.fail("Fill mass is missing, invalid or stale");
        };
        if let Some(i) = self.waiting {
            let acknowledged = input.valves[i].is_some_and(|v| {
                v.open == self.desired[i]
                    && v.at_ms >= self.command_since
                    && v.at_ms <= input.now_ms
            });
            if !acknowledged {
                if input.now_ms.saturating_sub(self.command_since) >= VALVE_ACK_TIMEOUT_MS {
                    return self.fail("Fill trim valve acknowledgement timed out");
                }
                return Update::default();
            }
            self.waiting = None;
            if self.pending != 0 {
                return Update {
                    effects: self.next(input.now_ms),
                    ..Update::default()
                };
            }
            self.since = input.now_ms;
            self.samples.clear();
            self.phase = match self.phase {
                Phase::VentOpening => Phase::Venting,
                _ => Phase::Settling,
            };
        }
        // Once configured, every valve must still report the requested state.
        if input.valves.iter().enumerate().any(|(i, v)| {
            !v.is_some_and(|v| {
                v.open == self.desired[i]
                    && v.at_ms <= input.now_ms
                    && input.now_ms - v.at_ms <= 2000
            })
        }) {
            return self.fail("Fill trim valve feedback is missing, stale or mismatched");
        }
        if self.phase == Phase::Venting {
            if input.now_ms.saturating_sub(self.since) >= PULSE_MS {
                return Update {
                    effects: self.configure(Phase::VentClosing, CLOSED, input.now_ms),
                    ..Update::default()
                };
            }
            return Update::default();
        }
        if self.phase != Phase::Settling {
            return Update::default();
        }
        // Only post-close samples contribute to the settled mass estimate.
        if mass.at_ms >= self.since && self.samples.last().is_none_or(|s| mass.at_ms > s.at_ms) {
            self.samples.push(mass);
        }
        self.samples
            .retain(|s| input.now_ms.saturating_sub(s.at_ms) <= 1000);
        if self.samples.len() > 1000 {
            self.samples.drain(..self.samples.len() - 1000);
        }
        if input.now_ms.saturating_sub(self.since) < self.settle_ms {
            return Update::default();
        }
        let stable = self.samples.len() >= 3
            && self.samples.last().unwrap().at_ms - self.samples[0].at_ms >= 800;
        let low = self
            .samples
            .iter()
            .map(|s| s.value * self.direction)
            .fold(f32::INFINITY, f32::min);
        let high = self
            .samples
            .iter()
            .map(|s| s.value * self.direction)
            .fold(f32::NEG_INFINITY, f32::max);
        if !stable || high - low > 2.0 * self.tolerance {
            if input.now_ms.saturating_sub(self.since) > self.settle_ms + 30000 {
                return self.fail("Fill mass did not settle");
            }
            return Update::default();
        }
        let mean = self
            .samples
            .iter()
            .map(|s| s.value * self.direction)
            .sum::<f32>()
            / self.samples.len() as f32;
        let target = self.target * self.direction;
        if (mean - target).abs() <= self.tolerance
            && (mass.value * self.direction - target).abs() <= self.tolerance
        {
            self.phase = Phase::Complete;
            return Update {
                completed: true,
                ..Update::default()
            };
        }
        if mean < target - self.tolerance {
            return self.fail("Settled fill mass is below target; manual review required");
        }
        if self.pulses >= MAX_PULSES {
            return self.fail("Fill trim pulse limit reached");
        }
        self.pulses += 1;
        let mut vent = CLOSED;
        vent[1] = true;
        Update {
            effects: self.configure(Phase::VentOpening, vent, input.now_ms),
            ..Update::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ValveState;
    struct Rig {
        controller: Controller,
        now: u64,
        valves: [bool; 5],
        mass: f32,
        completed: bool,
    }
    impl Rig {
        fn new(target: f32, mass: f32) -> Self {
            let (controller, effects) =
                Controller::start(100, target, 800.0, &Config::default()).unwrap();
            let mut this = Self {
                controller,
                now: 100,
                valves: [false, true, false, false, true],
                mass,
                completed: false,
            };
            this.apply(effects);
            this
        }
        fn apply(&mut self, effects: Effects) {
            for (i, value) in effects.valves {
                self.valves[i] = value;
            }
        }
        fn input(&self) -> Inputs {
            Inputs {
                now_ms: self.now,
                pressure: Some(Sample {
                    value: 500.0,
                    at_ms: self.now,
                }),
                valves: self.valves.map(|open| {
                    Some(ValveState {
                        open,
                        at_ms: self.now,
                    })
                }),
                interlock: true,
                prelaunch: true,
            }
        }
        fn tick(&mut self) -> Update {
            self.now += 100;
            let update = self.controller.tick(
                self.input(),
                Some(Sample {
                    value: self.mass,
                    at_ms: self.now,
                }),
            );
            self.completed |= update.completed;
            for &(i, value) in &update.effects.valves {
                self.valves[i] = value;
            }
            update
        }
        fn until(&mut self, phase: Phase) {
            for _ in 0..1000 {
                if self.controller.phase() == phase {
                    return;
                }
                let update = self.tick();
                assert!(update.fault.is_none(), "{:?}", update.fault);
            }
            panic!("phase {:?}", self.controller.phase());
        }
    }
    #[test]
    fn signed_targets_close_settle_then_complete_only_with_vent_closed() {
        for direction in [-1.0, 1.0] {
            let mut rig = Rig::new(10.0 * direction, 10.03 * direction);
            assert_eq!(rig.controller.stop_mass_kg(), 10.05 * direction);
            rig.until(Phase::Settling);
            let closed_at = rig.now;
            assert_eq!(rig.valves, CLOSED);
            while rig.now - closed_at < 2900 {
                assert!(!rig.tick().completed);
            }
            rig.until(Phase::Complete);
            assert!(rig.completed);
            assert_eq!(rig.valves, CLOSED);
        }
    }
    #[test]
    fn overshoot_pulses_vent_from_ack_then_closes_and_resettles_in_both_directions() {
        for direction in [-1.0, 1.0] {
            let mut rig = Rig::new(10.0 * direction, 10.2 * direction);
            rig.until(Phase::Venting);
            let opened_at = rig.now;
            assert!(rig.valves[1]);
            assert!(!rig.completed);
            let closed = rig.tick();
            assert_eq!(closed.effects.valves, vec![(1, false)]);
            assert!(rig.now - opened_at >= PULSE_MS);
            assert_eq!(rig.controller.phase(), Phase::VentClosing);
            rig.mass = 10.01 * direction;
            rig.until(Phase::Settling);
            let settled_from = rig.now;
            assert_eq!(rig.valves, CLOSED);
            rig.until(Phase::Complete);
            assert!(rig.now - settled_from >= 3000);
            assert_eq!(rig.valves, CLOSED);
        }
    }
    #[test]
    fn missing_predispatch_and_future_ack_cannot_release_configuration() {
        let mut rig = Rig::new(10.0, 10.0);
        for mode in 0..3 {
            let mut input = rig.input();
            input.valves[3] = match mode {
                0 => None,
                1 => Some(ValveState {
                    open: false,
                    at_ms: 99,
                }),
                _ => Some(ValveState {
                    open: false,
                    at_ms: 9999,
                }),
            };
            let update = rig.controller.tick(
                input,
                Some(Sample {
                    value: 10.0,
                    at_ms: rig.now,
                }),
            );
            assert!(update.effects.valves.is_empty());
            assert_eq!(rig.controller.phase(), Phase::Closing);
        }
        rig.now += VALVE_ACK_TIMEOUT_MS;
        let mut input = rig.input();
        input.valves[3] = None;
        let update = rig.controller.tick(
            input,
            Some(Sample {
                value: 10.0,
                at_ms: rig.now,
            }),
        );
        assert!(update.fault.is_some());
        assert!(update.effects.valves.iter().all(|(_, open)| !*open));
    }
    #[test]
    fn stale_mass_pressure_interlock_and_mismatched_valve_fail_closed() {
        for kind in 0..5 {
            let mut rig = Rig::new(10.0, 10.0);
            rig.until(Phase::Settling);
            rig.now += 2100;
            let mut input = rig.input();
            let mut mass = Some(Sample {
                value: 10.0,
                at_ms: rig.now,
            });
            match kind {
                0 => mass.as_mut().unwrap().at_ms -= 2100,
                1 => input.pressure = None,
                2 => input.interlock = false,
                3 => input.prelaunch = false,
                _ => input.valves[4].as_mut().unwrap().open = true,
            };
            let update = rig.controller.tick(input, mass);
            assert!(update.fault.is_some());
            assert_eq!(rig.controller.phase(), Phase::Fault);
            assert!(update.effects.valves.iter().all(|(_, open)| !*open));
        }
    }
    #[test]
    fn stale_repeated_mass_and_unstable_mass_never_qualify() {
        for repeated in [false, true] {
            let mut rig = Rig::new(10.0, 10.0);
            rig.until(Phase::Settling);
            let captured = rig.now;
            for index in 0..400 {
                rig.now += 100;
                let value = if index % 2 == 0 { 9.8 } else { 10.2 };
                let update = rig.controller.tick(
                    rig.input(),
                    Some(Sample {
                        value,
                        at_ms: if repeated { captured } else { rig.now },
                    }),
                );
                assert!(!update.completed);
                if update.fault.is_some() {
                    break;
                }
            }
            assert_eq!(rig.controller.phase(), Phase::Fault);
        }
    }
    #[test]
    fn underfill_and_bounded_repeated_pulses_do_not_reopen_supply() {
        let mut under = Rig::new(10.0, 9.8);
        under.until(Phase::Settling);
        for _ in 0..100 {
            let update = under.tick();
            assert!(
                update
                    .effects
                    .valves
                    .iter()
                    .all(|(i, open)| *i < 3 || !*open)
            );
            if update.fault.is_some() {
                break;
            }
        }
        assert_eq!(under.controller.phase(), Phase::Fault);
        assert!(!under.completed);
        let mut over = Rig::new(10.0, 10.2);
        for _ in 0..2000 {
            let update = over.tick();
            assert!(
                update
                    .effects
                    .valves
                    .iter()
                    .all(|(i, open)| *i < 3 || !*open)
            );
            if update.fault.is_some() {
                break;
            }
        }
        assert_eq!(over.controller.phase(), Phase::Fault);
        assert_eq!(over.controller.pulses, MAX_PULSES);
    }
    #[test]
    fn cancellation_is_pure_and_never_replays_outputs() {
        for phase in [
            Phase::Closing,
            Phase::Settling,
            Phase::VentOpening,
            Phase::Venting,
            Phase::VentClosing,
        ] {
            let mut rig = Rig::new(10.0, 10.2);
            rig.until(phase);
            let valves = rig.valves;
            rig.controller.cancel();
            for _ in 0..100 {
                let update = rig.tick();
                assert!(
                    update.effects.valves.is_empty() && !update.completed && update.fault.is_none()
                );
            }
            assert_eq!(rig.valves, valves);
        }
    }
}
