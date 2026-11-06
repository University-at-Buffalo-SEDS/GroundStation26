//! Ground-only cold flow. This controller never emits ignition, Launch or clocks.
//! Every output is a request; progress requires fresh, causally newer board reports.
use crate::{CLOSED, Effects, Inputs, RELIEVED, VALVE_ACK_TIMEOUT_MS};
use serde::{Deserialize, Serialize};

const FRESH_MS: u64 = 2000;
const ORDER: [usize; 5] = [3, 4, 0, 2, 1];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Isolating,
    Pressurizing,
    ConfirmingHold,
    AwaitingRelease,
    Releasing,
    Flowing,
    PauseClosing,
    ParkedUnverified,
    CancelClosing,
    CancelRelieving,
    Cancelled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    InvalidPressure,
    Interlock,
    PressureCeiling,
    ReadbackTimeout,
    ClockRegression,
    OperatorPause,
    UnexpectedNitrous,
    TransportUnavailable,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observation {
    pub phase: Phase,
    pub desired: [bool; 5],
    pub target_psi: f32,
    pub ceiling_psi: f32,
    pub pending_mask: u8,
    pub waiting: Option<usize>,
    pub command_since_ms: u64,
    pub issue: Option<Issue>,
    pub closure_confirmed: bool,
}
#[derive(Clone)]
pub struct Controller {
    phase: Phase,
    target: f32,
    ceiling: f32,
    desired: [bool; 5],
    pending: u8,
    waiting: Option<usize>,
    since: [u64; 5],
    command_since: u64,
    last_now: u64,
    issue: Option<Issue>,
    closure_confirmed: bool,
    resume_to_hold: bool,
}
/// Remaining validity of the actual calibrated pressure receipt used by start.
/// This never refreshes a producer timestamp or authorizes a command.
pub fn start_input_valid_for_ms(input: Inputs) -> u64 {
    input
        .pressure
        .filter(|sample| sample.value.is_finite() && sample.at_ms <= input.now_ms)
        .map_or(0, |sample| {
            FRESH_MS.saturating_sub(input.now_ms - sample.at_ms)
        })
}
fn pressure(input: Inputs) -> Option<f32> {
    input
        .pressure
        .filter(|s| {
            s.value.is_finite() && s.at_ms <= input.now_ms && input.now_ms - s.at_ms <= FRESH_MS
        })
        .map(|s| s.value)
}
impl Controller {
    /// Target and physical ceiling are captured once; no live retargeting.
    pub fn start(
        target: Option<f32>,
        ceiling: Option<f32>,
        input: Inputs,
    ) -> Result<(Self, Effects), &'static str> {
        let target = target
            .filter(|v| v.is_finite() && *v > 0.)
            .ok_or("Configure a separate cold-flow target")?;
        let ceiling = ceiling
            .filter(|v| v.is_finite() && *v > target)
            .ok_or("Cold-flow target must be below the configured hardware ceiling")?;
        if !input.prelaunch || !input.interlock {
            return Err("Cold flow requires current prelaunch interlocks");
        }
        if pressure(input).is_none_or(|v| v >= ceiling) {
            return Err("Cold flow requires fresh pressure below the hardware ceiling");
        }
        let mut this = Self {
            phase: Phase::Isolating,
            target,
            ceiling,
            desired: CLOSED,
            pending: 0,
            waiting: None,
            since: [input.now_ms; 5],
            command_since: input.now_ms,
            last_now: input.now_ms,
            issue: None,
            closure_confirmed: false,
            resume_to_hold: false,
        };
        let effects = this.configure(Phase::Isolating, CLOSED, input.now_ms);
        Ok((this, effects))
    }
    pub fn observation(&self) -> Observation {
        Observation {
            phase: self.phase,
            desired: self.desired,
            target_psi: self.target,
            ceiling_psi: self.ceiling,
            pending_mask: self.pending,
            waiting: self.waiting,
            command_since_ms: self.command_since,
            issue: self.issue,
            closure_confirmed: self.closure_confirmed,
        }
    }
    fn configure(&mut self, phase: Phase, desired: [bool; 5], now: u64) -> Effects {
        self.phase = phase;
        self.desired = desired;
        self.pending = 0b11111;
        self.waiting = None;
        self.since = [now; 5];
        self.closure_confirmed = false;
        self.next(now)
    }
    fn next(&mut self, now: u64) -> Effects {
        let next = ORDER
            .into_iter()
            .find(|i| self.pending & (1 << i) != 0 && !self.desired[*i])
            .or_else(|| ORDER.into_iter().find(|i| self.pending & (1 << i) != 0));
        self.waiting = next;
        if let Some(i) = next {
            self.pending &= !(1 << i);
            self.since[i] = now;
            self.command_since = now;
            Effects {
                valves: vec![(i, self.desired[i])],
                notification: None,
            }
        } else {
            Effects::default()
        }
    }
    fn next_checked(&mut self, input: Inputs) -> Effects {
        let closing_pending = ORDER
            .into_iter()
            .any(|i| self.pending & (1 << i) != 0 && !self.desired[i]);
        if !closing_pending {
            // Before EVERY opening, verify all other requested states now,
            // including source/PILOT closures before either relief path opens.
            if let Some(next) = ORDER.into_iter().find(|i| self.pending & (1 << i) != 0)
                && !self.opening_readbacks_current(input, next)
            {
                self.issue = Some(Issue::ReadbackTimeout);
                self.closure_confirmed = false;
                return Effects::default();
            }
        }
        self.next(input.now_ms)
    }
    /// Exact original opening proof. Retried admission must retain each
    /// command's capture floor; age alone cannot replace causal evidence.
    pub fn opening_readbacks_current(&self, input: Inputs, next: usize) -> bool {
        next < 5
            && !(0..5).any(|i| {
                if i == next {
                    return false;
                }
                if self.phase == Phase::CancelRelieving
                    && self.desired[i]
                    && self.pending & (1 << i) != 0
                {
                    return false;
                }
                let expected = self.desired[i] && self.pending & (1 << i) == 0;
                !input.valves[i].is_some_and(|v| {
                    v.open == expected
                        && v.at_ms > self.since[i]
                        && v.at_ms <= input.now_ms
                        && input.now_ms - v.at_ms <= FRESH_MS
                })
            })
    }
    fn reported(&self, input: Inputs, i: usize) -> bool {
        input.valves[i].is_some_and(|v| {
            v.open == self.desired[i]
                && v.at_ms > self.since[i]
                && v.at_ms <= input.now_ms
                && input.now_ms - v.at_ms <= FRESH_MS
        })
    }
    fn all_reported(&self, input: Inputs) -> bool {
        (0..5).all(|i| self.reported(input, i))
    }
    /// Internal parking interface. The shared supervisor owns pause identity,
    /// authorization and admission. Safety/readback clocks continue uninterrupted.
    pub fn park(&mut self, now: u64) -> Effects {
        if matches!(
            self.phase,
            Phase::CancelClosing | Phase::CancelRelieving | Phase::Cancelled
        ) {
            return Effects::default();
        }
        self.resume_to_hold |= matches!(
            self.phase,
            Phase::AwaitingRelease | Phase::Releasing | Phase::Flowing
        );
        self.issue = Some(Issue::OperatorPause);
        self.close_batch(Phase::PauseClosing, now, true)
    }
    pub fn park_for_transport(&mut self, now: u64) -> Effects {
        let effects = self.park(now);
        self.issue = Some(Issue::TransportUnavailable);
        effects
    }
    pub fn paused_ready(&self) -> bool {
        // Fresh closed reports cannot retire an older SDK/board-owned open.
        // Shared pause supervisor must supply a qualified owner barrier before
        // this controller can expose a resumable Paused state.
        false
    }
    /// Caller must supply a newly validated shared-supervisor admission. This
    /// pure precondition check is not authorization. Never restores an open PILOT.
    pub fn resume(&mut self, input: Inputs) -> Result<Effects, &'static str> {
        if !self.paused_ready()
            || !self.all_reported(input)
            || !input.prelaunch
            || !input.interlock
            || pressure(input).is_none_or(|p| p >= self.ceiling)
            || input.now_ms < self.last_now
        {
            return Err(
                "Cold flow cannot resume without current isolation, pressure and admission",
            );
        }
        self.issue = None;
        self.last_now = input.now_ms;
        Ok(self.configure(
            if self.resume_to_hold {
                Phase::ConfirmingHold
            } else {
                Phase::Isolating
            },
            CLOSED,
            input.now_ms,
        ))
    }
    /// Run/actor/epoch authorization lives at the actual server dispatch boundary.
    pub fn release(&mut self, input: Inputs) -> Result<Effects, &'static str> {
        if self.phase != Phase::AwaitingRelease
            || self.waiting.is_some()
            || self.pending != 0
            || !self.all_reported(input)
            || !input.prelaunch
            || !input.interlock
            || input.now_ms < self.last_now
            || pressure(input).is_none_or(|p| p >= self.ceiling)
        {
            return Err("Cold-flow release requires the current isolated awaiting-release run");
        }
        self.last_now = input.now_ms;
        // Isolation has just been freshly verified by this explicit release.
        // Admit ONLY PILOT now; do not defer its opening into later autonomous
        // ticks after the operator's authorization has expired/revoked.
        self.phase = Phase::Releasing;
        self.desired = [true, false, false, false, false];
        self.pending = 1;
        self.waiting = None;
        self.closure_confirmed = false;
        Ok(self.next_checked(input))
    }
    /// Safe cancellation is independent of pressure validity and initiator session.
    /// Never opens a relief path before supplies and PILOT are freshly closed.
    pub fn cancel(&mut self, now: u64) -> Effects {
        if matches!(
            self.phase,
            Phase::CancelClosing | Phase::CancelRelieving | Phase::Cancelled
        ) {
            return Effects::default();
        }
        self.close_batch(Phase::CancelClosing, now, false)
    }
    fn close_batch(&mut self, phase: Phase, now: u64, all: bool) -> Effects {
        self.phase = phase;
        self.desired = CLOSED;
        self.pending = 0;
        self.waiting = None;
        self.command_since = now;
        self.closure_confirmed = false;
        let indices: &[usize] = if all { &[3, 4, 0, 2, 1] } else { &[3, 4, 0] };
        for &i in indices {
            self.since[i] = now;
        }
        Effects {
            valves: indices.iter().map(|i| (*i, false)).collect(),
            notification: None,
        }
    }
    pub fn tick(&mut self, input: Inputs) -> Effects {
        if input.now_ms < self.last_now {
            self.issue = Some(Issue::ClockRegression);
            // No output admission from a regressed clock; caller must resolve epoch.
            self.closure_confirmed = false;
            return Effects::default();
        }
        self.last_now = input.now_ms;
        if self.phase == Phase::Cancelled {
            return Effects::default();
        }
        let cancelling = matches!(self.phase, Phase::CancelClosing | Phase::CancelRelieving);
        let parked = matches!(self.phase, Phase::PauseClosing | Phase::ParkedUnverified);
        if !cancelling && !parked {
            if input.valves[4].is_some_and(|v| {
                v.open && v.at_ms <= input.now_ms && input.now_ms - v.at_ms <= FRESH_MS
            }) {
                let effects = self.park(input.now_ms);
                self.issue = Some(Issue::UnexpectedNitrous);
                return effects;
            }
            let issue = if !input.prelaunch || !input.interlock {
                Some(Issue::Interlock)
            } else if pressure(input).is_none() {
                Some(Issue::InvalidPressure)
            } else {
                None
            };
            if let Some(issue) = issue {
                let effects = self.park(input.now_ms);
                self.issue = Some(issue);
                return effects;
            }
            if pressure(input).is_some_and(|p| p >= self.ceiling) {
                self.issue = Some(Issue::PressureCeiling);
                return self.cancel(input.now_ms);
            }
            // This must precede acknowledgement processing: target crossing can
            // occur before the outstanding N2-open report arrives.
            if self.phase == Phase::Pressurizing
                && pressure(input).is_some_and(|p| p >= self.target)
            {
                return self.configure(Phase::ConfirmingHold, CLOSED, input.now_ms);
            }
        }
        if self.phase == Phase::CancelClosing
            && [0, 3, 4].into_iter().all(|i| self.reported(input, i))
        {
            self.phase = Phase::CancelRelieving;
            self.desired = RELIEVED;
            self.pending = (1 << 1) | (1 << 2);
            self.waiting = None;
            return self.next_checked(input);
        }
        if matches!(self.phase, Phase::CancelClosing | Phase::PauseClosing)
            && input.now_ms.saturating_sub(self.command_since) > VALVE_ACK_TIMEOUT_MS
        {
            self.issue = Some(Issue::ReadbackTimeout);
        }
        if let Some(i) = self.waiting {
            if self.reported(input, i) {
                self.waiting = None;
                if self.pending != 0 {
                    return self.next_checked(input);
                }
            } else {
                if input.now_ms.saturating_sub(self.command_since) > VALVE_ACK_TIMEOUT_MS {
                    self.issue = Some(Issue::ReadbackTimeout);
                    self.closure_confirmed = false;
                    if !cancelling && !parked {
                        let effects = self.park(input.now_ms);
                        self.issue = Some(Issue::ReadbackTimeout);
                        return effects;
                    }
                }
                return Effects::default();
            }
        }
        if self.pending != 0 {
            return self.next_checked(input);
        }
        if !self.all_reported(input) {
            self.closure_confirmed = false;
            if matches!(
                self.phase,
                Phase::AwaitingRelease | Phase::Flowing | Phase::Pressurizing | Phase::Releasing
            ) {
                let effects = self.park(input.now_ms);
                self.issue = Some(Issue::ReadbackTimeout);
                return effects;
            }
            return Effects::default();
        }
        self.closure_confirmed = self.desired == CLOSED;
        match self.phase {
            Phase::Isolating => {
                if pressure(input).is_some_and(|p| p >= self.target) {
                    self.configure(Phase::ConfirmingHold, CLOSED, input.now_ms)
                } else {
                    self.configure(
                        Phase::Pressurizing,
                        [false, false, false, true, false],
                        input.now_ms,
                    )
                }
            }
            Phase::ConfirmingHold => {
                self.phase = Phase::AwaitingRelease;
                Effects::default()
            }
            Phase::Releasing => {
                self.phase = Phase::Flowing;
                Effects::default()
            }
            Phase::PauseClosing => {
                self.phase = Phase::ParkedUnverified;
                Effects::default()
            }
            Phase::CancelClosing => self.configure(Phase::CancelRelieving, RELIEVED, input.now_ms),
            Phase::CancelRelieving => {
                self.phase = Phase::Cancelled;
                Effects::default()
            }
            _ => Effects::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Sample, ValveState};
    struct Rig {
        c: Controller,
        input: Inputs,
        requested: [bool; 5],
        trace: Vec<(usize, bool)>,
    }
    impl Rig {
        fn new() -> Self {
            let input = Inputs {
                now_ms: 100,
                pressure: Some(Sample {
                    value: 0.,
                    at_ms: 100,
                }),
                valves: [Some(ValveState {
                    open: false,
                    at_ms: 100,
                }); 5],
                interlock: true,
                prelaunch: true,
            };
            let (c, e) = Controller::start(Some(100.), Some(200.), input).unwrap();
            let mut r = Self {
                c,
                input,
                requested: CLOSED,
                trace: Vec::new(),
            };
            r.apply(e);
            r
        }
        fn apply(&mut self, e: Effects) {
            assert!(e.valves.len() <= 5);
            for (i, on) in e.valves {
                self.requested[i] = on;
                self.trace.push((i, on));
            }
        }
        fn step(&mut self) {
            self.input.now_ms += 10;
            self.input.pressure.as_mut().unwrap().at_ms = self.input.now_ms;
            self.input.valves = std::array::from_fn(|i| {
                Some(ValveState {
                    open: self.requested[i],
                    at_ms: self.input.now_ms,
                })
            });
            let e = self.c.tick(self.input);
            self.apply(e);
        }
        fn until(&mut self, p: Phase) {
            for _ in 0..100 {
                if self.c.phase == p && self.c.waiting.is_none() {
                    return;
                }
                self.step();
            }
            panic!("phase {:?}, wanted {p:?}", self.c.phase);
        }
        fn hold(&mut self) {
            self.until(Phase::Pressurizing);
            self.input.pressure.as_mut().unwrap().value = 100.;
            self.step();
            self.until(Phase::AwaitingRelease);
        }
    }
    #[test]
    fn unset_target_and_invalid_limits_disable_without_copying_other_targets() {
        let r = Rig::new();
        for target in [None, Some(0.), Some(f32::NAN), Some(200.), Some(300.)] {
            assert!(Controller::start(target, Some(200.), r.input).is_err());
        }
        assert!(Controller::start(Some(100.), None, r.input).is_err());
        let mut input = r.input;
        input.pressure = None;
        assert!(Controller::start(Some(100.), Some(200.), input).is_err());
    }
    #[test]
    fn isolation_then_nitrogen_then_indefinite_hold_without_topup_or_ignition() {
        let mut r = Rig::new();
        r.until(Phase::Pressurizing);
        assert_eq!(
            r.trace
                .iter()
                .filter(|(_, on)| *on)
                .copied()
                .collect::<Vec<_>>(),
            vec![(3, true)]
        );
        assert_eq!(
            &r.trace[..5],
            &[(3, false), (4, false), (0, false), (2, false), (1, false)]
        );
        r.hold();
        assert_eq!(r.requested, CLOSED);
        let count = r.trace.len();
        r.input.pressure.as_mut().unwrap().value = 90.;
        for _ in 0..100 {
            r.step();
        }
        assert_eq!(r.c.phase, Phase::AwaitingRelease);
        assert_eq!(r.trace.len(), count);
    }
    #[test]
    fn target_crossing_before_nitrogen_open_readback_requests_close_first() {
        let mut r = Rig::new();
        r.until(Phase::Pressurizing);
        r.input.pressure.as_mut().unwrap().value = 101.;
        r.input.now_ms += 1;
        r.input.pressure.as_mut().unwrap().at_ms = r.input.now_ms;
        let e = r.c.tick(r.input);
        assert_eq!(e.valves, vec![(3, false)]);
        assert_eq!(r.c.phase, Phase::ConfirmingHold);
    }
    #[test]
    fn explicit_release_is_one_shot_and_board_report_is_not_measured_flow() {
        let mut r = Rig::new();
        assert!(r.c.release(r.input).is_err());
        r.hold();
        let e = r.c.release(r.input).unwrap();
        assert_eq!(e.valves, vec![(0, true)]);
        r.apply(e);
        assert!(r.c.release(r.input).is_err());
        r.until(Phase::Flowing);
        assert_eq!(r.requested, [true, false, false, false, false]);
        assert_eq!(r.trace.last(), Some(&(0, true)));
        for _ in 0..20 {
            r.step();
        }
        assert_eq!(r.c.phase, Phase::Flowing);
    }
    #[test]
    fn cancellation_never_uses_main_discharge_and_requires_real_new_readbacks() {
        let mut r = Rig::new();
        r.hold();
        let e = r.c.release(r.input).unwrap();
        r.apply(e);
        r.until(Phase::Flowing);
        let count = r.trace.len();
        let e = r.c.cancel(r.input.now_ms);
        r.apply(e);
        assert_eq!(&r.trace[count..], &[(3, false), (4, false), (0, false)]);
        // ACK without a newer board report cannot advance even the first close.
        let now = r.input.now_ms;
        r.input.now_ms = now + 1;
        assert!(r.c.tick(r.input).valves.is_empty());
        assert_eq!(r.c.phase, Phase::CancelClosing);
        r.until(Phase::Cancelled);
        let tail = &r.trace[count..];
        assert!(!tail.contains(&(0, true)));
        let pilot_closed = tail.iter().position(|x| *x == (0, false)).unwrap();
        let relief = tail
            .iter()
            .position(|(i, on)| *on && [1, 2].contains(i))
            .unwrap();
        assert!(pilot_closed < relief);
        assert_eq!(r.requested, RELIEVED);
    }
    #[test]
    fn missing_cancel_close_report_never_opens_dump_or_vent_or_claims_cancelled() {
        let mut r = Rig::new();
        r.hold();
        let e = r.c.cancel(r.input.now_ms);
        r.apply(e);
        r.input.now_ms += VALVE_ACK_TIMEOUT_MS + 1;
        r.input.pressure = None;
        assert!(r.c.tick(r.input).valves.is_empty());
        assert_eq!(r.c.phase, Phase::CancelClosing);
        assert_eq!(r.c.issue, Some(Issue::ReadbackTimeout));
        assert!(!r.c.closure_confirmed);
        assert!(!r.trace.iter().any(|(i, on)| *on && [0, 1, 2].contains(i)));
    }
    #[test]
    fn stale_nonfinite_future_pressure_and_interlock_loss_only_park_and_close() {
        for mode in 0..5 {
            let mut r = Rig::new();
            r.until(Phase::Pressurizing);
            r.input.now_ms += 3000;
            match mode {
                0 => r.input.pressure = None,
                1 => r.input.pressure.as_mut().unwrap().value = f32::NAN,
                2 => r.input.pressure.as_mut().unwrap().at_ms = r.input.now_ms + 1,
                3 => r.input.interlock = false,
                _ => {}
            }
            let e = r.c.tick(r.input);
            assert_eq!(
                e.valves,
                vec![(3, false), (4, false), (0, false), (2, false), (1, false)]
            );
            assert_eq!(r.c.phase, Phase::PauseClosing);
            assert!(r.c.release(r.input).is_err());
        }
    }
    #[test]
    fn hardware_ceiling_enters_safe_cancel_not_automatic_abort() {
        let mut r = Rig::new();
        r.until(Phase::Pressurizing);
        r.input.pressure.as_mut().unwrap().value = 200.;
        r.step();
        assert_eq!(r.c.phase, Phase::CancelClosing);
        assert_eq!(r.c.issue, Some(Issue::PressureCeiling));
        assert_eq!(
            &r.trace[r.trace.len() - 3..],
            &[(3, false), (4, false), (0, false)]
        );
        r.until(Phase::Cancelled);
    }
    #[test]
    fn parked_closed_reports_do_not_claim_paused_or_resume_without_owner_barrier() {
        let mut r = Rig::new();
        r.hold();
        let e = r.c.release(r.input).unwrap();
        r.apply(e);
        r.until(Phase::Flowing);
        let e = r.c.park(r.input.now_ms);
        r.apply(e);
        r.until(Phase::ParkedUnverified);
        assert!(!r.c.paused_ready());
        assert_eq!(r.requested, CLOSED);
        let count = r.trace.len();
        assert!(r.c.resume(r.input).is_err());
        assert_eq!(r.trace.len(), count);
        assert!(r.c.release(r.input).is_err());
    }
    #[test]
    fn getter_is_copy_only_and_regressed_clock_never_admits_an_open() {
        let mut r = Rig::new();
        let a = r.c.observation();
        assert_eq!(a, r.c.observation());
        r.input.now_ms -= 1;
        let e = r.c.tick(r.input);
        assert!(e.valves.is_empty());
        assert_eq!(r.c.issue, Some(Issue::ClockRegression));
        assert!(!r.c.closure_confirmed);
    }
    #[test]
    fn missing_nitrogen_report_still_requests_nitrous_and_pilot_close_but_never_relief() {
        let mut r = Rig::new();
        r.hold();
        let e = r.c.release(r.input).unwrap();
        r.apply(e);
        r.until(Phase::Flowing);
        let count = r.trace.len();
        let e = r.c.cancel(r.input.now_ms);
        r.apply(e);
        assert_eq!(&r.trace[count..], &[(3, false), (4, false), (0, false)]);
        r.input.now_ms += 10;
        r.input.valves[3] = None;
        r.input.valves[4] = Some(ValveState {
            open: false,
            at_ms: r.input.now_ms,
        });
        r.input.valves[0] = Some(ValveState {
            open: false,
            at_ms: r.input.now_ms,
        });
        assert!(r.c.tick(r.input).valves.is_empty());
        assert_eq!(r.c.phase, Phase::CancelClosing);
        r.input.now_ms += VALVE_ACK_TIMEOUT_MS + 1;
        assert!(r.c.tick(r.input).valves.is_empty());
        assert_eq!(r.c.issue, Some(Issue::ReadbackTimeout));
        assert!(!r.trace[count..].iter().any(|(_, on)| *on));
    }
    #[test]
    fn unexpected_nitrous_uses_typed_parking_without_legacy_nitrous_fill_or_abort() {
        let mut r = Rig::new();
        r.until(Phase::Pressurizing);
        r.input.now_ms += 1;
        r.input.valves[4] = Some(ValveState {
            open: true,
            at_ms: r.input.now_ms,
        });
        let e = r.c.tick(r.input);
        assert_eq!(r.c.phase, Phase::PauseClosing);
        assert_eq!(r.c.issue, Some(Issue::UnexpectedNitrous));
        assert_eq!(e.valves.len(), 5);
        assert!(e.valves.iter().all(|(_, on)| !*on));
        assert!(!r.c.paused_ready());
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;
    #[test]
    fn remaining_start_lease_uses_original_pressure_receipt() {
        let mut input = Inputs {
            now_ms: 10000,
            pressure: Some(crate::Sample {
                value: 42.,
                at_ms: 9000,
            }),
            valves: [None; 5],
            prelaunch: true,
            interlock: true,
        };
        assert_eq!(start_input_valid_for_ms(input), 1000);
        input.now_ms = 11000;
        assert_eq!(start_input_valid_for_ms(input), 0);
        input.now_ms = 12000;
        assert_eq!(start_input_valid_for_ms(input), 0);
        input.now_ms = 8000;
        assert_eq!(start_input_valid_for_ms(input), 0);
        input.pressure.as_mut().unwrap().value = f32::NAN;
        assert_eq!(start_input_valid_for_ms(input), 0);
    }
}
