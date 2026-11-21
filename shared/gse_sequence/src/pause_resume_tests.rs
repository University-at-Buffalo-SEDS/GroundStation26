//! Operator pause, resume and cancel for the valve self-test and nitrogen test.
use super::*;

struct Rig {
    engine: Engine,
    now: u64,
    pressure: f32,
    valves: [bool; 5],
    /// Every valve command in order.
    trace: Vec<(usize, bool)>,
    report_fresh: bool,
}
impl Rig {
    fn new() -> Self {
        let engine = Engine {
            config: Config {
                nitrogen_target_psi: 120.0,
                pressure_step_psi: 50.0,
                pressure_ceiling_psi: Some(200.0),
                maximum_zero_offset_psi: Some(8.0),
                dry_self_test_confirmed: true,
                ..Config::default()
            },
            ..Engine::default()
        };
        Self {
            engine,
            now: 100,
            pressure: 0.0,
            valves: RELIEVED,
            trace: Vec::new(),
            report_fresh: true,
        }
    }
    fn input(&self) -> Inputs {
        let at = if self.report_fresh { self.now } else { self.now.saturating_sub(5000) };
        Inputs {
            now_ms: self.now,
            pressure: Some(Sample {
                value: self.pressure,
                at_ms: self.now,
            }),
            valves: std::array::from_fn(|i| {
                Some(ValveState {
                    open: self.valves[i],
                    at_ms: at,
                })
            }),
            interlock: true,
            prelaunch: true,
        }
    }
    fn apply(&mut self, effects: Effects) {
        for (i, open) in effects.valves {
            self.valves[i] = open;
            self.trace.push((i, open));
        }
    }
    fn request(&mut self, action: Action) {
        let effects = self.engine.request(action, self.input()).unwrap();
        self.apply(effects);
    }
    fn resume(&mut self) {
        let effects = self.engine.resume_test(self.input()).unwrap();
        self.apply(effects);
    }
    fn dump(&mut self) {
        let effects = self.engine.dump_test_hold(self.input()).unwrap();
        self.apply(effects);
    }
    fn tick(&mut self) {
        self.now += 100;
        let effects = self.engine.tick(self.input());
        self.apply(effects);
    }
    fn until(&mut self, phase: Phase) {
        for _ in 0..2000 {
            if self.engine.status.phase == phase
                && self.engine.self_pending == 0
                && self.engine.self_waiting.is_none()
            {
                return;
            }
            assert_ne!(self.engine.status.phase, Phase::Fault, "{:?}", self.engine.status);
            self.tick();
        }
        panic!("did not reach {phase:?}: {:?}", self.engine.status);
    }
    fn until_raising(&mut self, step: usize) {
        for _ in 0..2000 {
            if self.engine.status.phase == Phase::Raising && self.engine.nitrogen_step_index == step {
                return;
            }
            assert_ne!(self.engine.status.phase, Phase::Fault, "{:?}", self.engine.status);
            self.tick();
        }
        panic!("did not raise step {step}: {:?}", self.engine.status);
    }
    fn pause(&mut self) {
        assert!(self.engine.allows(Action::PauseFill, self.input()));
        self.trace.clear();
        self.request(Action::PauseFill);
        self.until(Phase::Paused);
        assert_eq!(self.valves, CLOSED);
        // Supplies close before any relief valve moves.
        let first: Vec<_> = self.trace.iter().take(2).map(|(i, _)| *i).collect();
        assert!(first.iter().all(|i| *i >= 3) || self.trace.len() < 2);
        assert!(self.trace.iter().all(|(_, open)| !open), "pause never opens");
    }
}

#[test]
fn every_test_phase_can_pause_and_fill_cannot_start_from_a_paused_test() {
    for phase in [
        Phase::BaselineSetup,
        Phase::Baseline,
        Phase::SelfSetup,
        Phase::SelfOpen,
        Phase::SelfHold,
        Phase::SelfClose,
        Phase::SelfFinish,
        Phase::NitrogenSetup,
        Phase::Raising,
        Phase::Settling,
        Phase::Holding,
        Phase::Dumping,
    ] {
        let mut rig = Rig::new();
        rig.engine.status.phase = phase;
        assert!(rig.engine.allows(Action::PauseFill, rig.input()), "{phase:?}");
    }
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    rig.pause();
    assert_eq!(rig.engine.paused_test(), Some(PausedTest::NitrogenTest));
    assert!(rig.engine.status.message.contains("Nitrogen test paused"));
    // A paused test never satisfies fill, even if an override is later recorded.
    rig.engine.status.nitrogen_overridden = true;
    assert!(!rig.engine.allows(Action::StartFill, rig.input()));
    assert!(!rig.engine.allows(Action::NitrogenTest, rig.input()));
    assert!(!rig.engine.allows(Action::SelfTest, rig.input()));
}

#[test]
fn valve_self_test_resumes_at_the_current_valve_without_retesting_earlier_valves() {
    let mut rig = Rig::new();
    rig.request(Action::SelfTest);
    // Run until valve 2 (dump) is in its dwell.
    for _ in 0..2000 {
        if rig.engine.status.phase == Phase::SelfHold && rig.engine.self_index == 2 {
            break;
        }
        rig.tick();
    }
    assert_eq!(rig.engine.self_index, 2);
    rig.pause();
    assert_eq!(rig.engine.paused_test(), Some(PausedTest::ValveSelfTest));
    // Re-pausing a paused test keeps the resume point.
    rig.request(Action::PauseFill);
    rig.until(Phase::Paused);
    assert_eq!(rig.engine.paused_test(), Some(PausedTest::ValveSelfTest));
    rig.trace.clear();
    rig.resume();
    let mut moved = Vec::new();
    for _ in 0..2000 {
        if rig.engine.status.phase == Phase::SelfHold {
            let valve = (0..5).find(|i| rig.valves[*i] != RELIEVED[*i]).unwrap();
            if !moved.contains(&valve) {
                moved.push(valve);
            }
        }
        if rig.engine.status.phase == Phase::Idle {
            break;
        }
        assert_ne!(rig.engine.status.phase, Phase::Fault);
        rig.tick();
    }
    assert_eq!(moved, vec![2, 3, 4], "resume re-tests the current valve onward");
    assert_eq!(rig.valves, RELIEVED);
    assert_eq!(rig.engine.status.phase, Phase::Idle);
    assert!(rig.engine.paused_test().is_none());
}

#[test]
fn nitrogen_resume_restarts_the_current_step_and_its_hold_timer() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    // Complete step 0 (50 psi) and pause while raising step 1.
    rig.pressure = 52.0;
    rig.until(Phase::Holding);
    rig.until_raising(1);
    rig.pressure = 70.0;
    rig.pause();
    assert_eq!(rig.engine.nitrogen_step_index, 1);
    rig.trace.clear();
    rig.resume();
    rig.until(Phase::Raising);
    assert_eq!(rig.engine.nitrogen_step_index, 1, "same step resumes");
    assert!(rig.valves[3], "nitrogen reopens only after closures are confirmed");
    rig.pressure = 102.0;
    rig.until(Phase::Holding);
    let hold_started = rig.engine.phase_since;
    rig.until_raising(2);
    assert!(rig.now - hold_started >= 5000, "full hold after resume");
}

#[test]
fn nitrogen_resume_above_the_step_settles_without_reopening_nitrogen() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    rig.pressure = 52.0;
    rig.until(Phase::Holding);
    for _ in 0..10 {
        rig.tick();
    }
    rig.pause();
    rig.trace.clear();
    rig.resume();
    assert_eq!(rig.engine.status.phase, Phase::Settling);
    rig.until(Phase::Holding);
    assert!(rig.trace.iter().all(|(i, open)| *i != 3 || !open));
    assert_eq!(rig.engine.nitrogen_step_index, 0);
}

#[test]
fn pause_during_baseline_repeats_baseline_and_pause_while_dumping_resumes_dumping() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Baseline);
    for _ in 0..10 {
        rig.tick();
    }
    rig.pause();
    rig.resume();
    assert_eq!(rig.engine.status.phase, Phase::BaselineSetup);
    assert!(rig.engine.status.baseline.is_none());
    rig.until(Phase::Baseline);
    rig.until(Phase::Raising);
    for (step, target) in [52.0, 102.0, 122.0].into_iter().enumerate() {
        rig.pressure = target;
        rig.until(Phase::Holding);
        if step < 2 {
            rig.until_raising(step + 1);
        }
    }
    // The passed test holds its top-step pressure until the operator dumps.
    rig.until(Phase::Paused);
    assert_eq!(rig.engine.test_hold(), Some(&TestHold::Passed));
    rig.dump();
    rig.until(Phase::Dumping);
    rig.pressure = 60.0;
    rig.pause();
    rig.resume();
    assert_eq!(rig.engine.status.phase, Phase::Dumping);
    rig.pressure = 0.0;
    rig.until(Phase::Passed);
    assert!(rig.engine.status.nitrogen_passed);
}

#[test]
fn resume_requires_fresh_closed_reports_interlocks_and_pressure_and_cancel_relieves() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    rig.pressure = 30.0;
    rig.pause();
    rig.report_fresh = false;
    assert!(rig.engine.resume_test_check(rig.input()).is_err());
    rig.report_fresh = true;
    let mut input = rig.input();
    input.interlock = false;
    assert!(rig.engine.resume_test(input).is_err());
    input = rig.input();
    input.prelaunch = false;
    assert!(rig.engine.resume_test(input).is_err());
    input = rig.input();
    input.pressure = None;
    assert!(rig.engine.resume_test(input).is_err());
    rig.pressure = 220.0;
    assert!(rig.engine.resume_test_check(rig.input()).is_err());
    rig.pressure = 30.0;
    assert!(rig.engine.resume_test_check(rig.input()).is_ok());
    assert_eq!(rig.engine.status.phase, Phase::Paused, "refusals change nothing");
    rig.request(Action::CancelFill);
    rig.until(Phase::Cancelled);
    assert_eq!(rig.valves, RELIEVED);
    assert!(rig.engine.paused_test().is_none());
    assert!(rig.engine.resume_test_check(rig.input()).is_err());
}

#[test]
fn fill_pause_is_not_a_test_pause_and_cannot_resume_as_one() {
    let mut rig = Rig::new();
    rig.engine.status.phase = Phase::Filling;
    rig.engine.status.nitrogen_passed = true;
    rig.request(Action::PauseFill);
    rig.until(Phase::Paused);
    assert!(rig.engine.paused_test().is_none());
    assert!(rig.engine.resume_test_check(rig.input()).is_err());
    assert!(rig.engine.status.message.contains("Fill paused"));
}
