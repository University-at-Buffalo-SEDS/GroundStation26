//! Non-safety failures pause with every valve closed; hazards keep relief.
use super::*;

struct Rig {
    engine: Engine,
    now: u64,
    pressure: f32,
    pressure_fresh: bool,
    valves: [bool; 5],
    /// Valves whose board ignores commands (stuck at their current state).
    stuck: [bool; 5],
    trace: Vec<(usize, bool)>,
    notes: Vec<String>,
}
impl Rig {
    fn new() -> Self {
        Self {
            engine: Engine {
                config: Config {
                    nitrogen_target_psi: 120.0,
                    pressure_step_psi: 50.0,
                    pressure_ceiling_psi: Some(200.0),
                    maximum_zero_offset_psi: Some(8.0),
                    dry_self_test_confirmed: true,
                    ..Config::default()
                },
                ..Engine::default()
            },
            now: 100,
            pressure: 0.0,
            pressure_fresh: true,
            valves: RELIEVED,
            stuck: [false; 5],
            trace: Vec::new(),
            notes: Vec::new(),
        }
    }
    fn input(&self) -> Inputs {
        Inputs {
            now_ms: self.now,
            pressure: self.pressure_fresh.then_some(Sample {
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
    fn apply(&mut self, effects: Effects) {
        for (i, open) in effects.valves {
            self.trace.push((i, open));
            if !self.stuck[i] {
                self.valves[i] = open;
            }
        }
        self.notes.extend(effects.notification);
    }
    fn request(&mut self, action: Action) {
        let effects = self.engine.request(action, self.input()).unwrap();
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
            self.tick();
        }
        panic!("did not reach {phase:?}: {:?}", self.engine.status);
    }
    /// A qualified fill in progress, with the tank holding nitrous.
    fn filling(&mut self) {
        self.engine.status.baseline = Some(Baseline {
            average_psi: 0.0,
            min_psi: -1.0,
            max_psi: 1.0,
            noise_psi: 1.0,
            samples: 50,
        });
        self.engine.status.nitrogen_passed = true;
        self.engine.status.phase = Phase::Passed;
        self.engine.nitrous_target_psi = 140.0;
        self.request(Action::StartFill);
        self.until(Phase::Filling);
        assert_eq!(self.valves, [false, true, false, false, true]);
        self.pressure = 60.0;
        self.trace.clear();
        self.notes.clear();
    }
    fn opened_relief_since_failure(&self) -> bool {
        self.trace.iter().any(|&(i, open)| open && (i == 1 || i == 2))
    }
}

#[test]
fn stale_pressure_during_fill_pauses_closed_and_resumes_on_operator_request() {
    let mut rig = Rig::new();
    rig.filling();
    rig.pressure_fresh = false;
    rig.tick();
    // Both supplies close at once, nitrogen first.
    assert_eq!(rig.trace[..2], [(3, false), (4, false)]);
    assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
    rig.pressure_fresh = true;
    rig.until(Phase::Paused);
    assert_eq!(rig.valves, CLOSED);
    assert!(!rig.opened_relief_since_failure(), "{:?}", rig.trace);
    assert_eq!(
        rig.engine.failure(),
        Some("Tank pressure is missing, invalid or stale")
    );
    assert_eq!(
        rig.notes.first().map(String::as_str),
        Some("Sequence paused: Tank pressure is missing, invalid or stale. Review and Resume or Cancel.")
    );
    assert!(rig.notes.last().unwrap().contains("all valves are confirmed closed"));
    for _ in 0..50 {
        rig.tick();
        assert_eq!(rig.engine.status.phase, Phase::Paused);
        assert_eq!(rig.valves, CLOSED);
    }
    // Resume is the ordinary paused-fill StartFill and acknowledges the failure.
    rig.request(Action::StartFill);
    assert!(rig.engine.failure().is_none());
    rig.until(Phase::Filling);
    // Cancel remains the explicit dump.
    rig.request(Action::CancelFill);
    rig.until(Phase::Cancelled);
    assert_eq!(rig.valves, RELIEVED);
}

#[test]
fn pause_closure_timeout_keeps_closing_without_venting_or_dumping() {
    let mut rig = Rig::new();
    rig.filling();
    rig.stuck[0] = true;
    rig.valves[0] = true; // pilot never confirms closed
    rig.pressure_fresh = false;
    rig.tick();
    rig.pressure_fresh = true;
    for _ in 0..200 {
        rig.tick();
        assert_ne!(rig.engine.status.phase, Phase::Fault, "{:?}", rig.engine.status);
    }
    assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
    assert!(!rig.valves[3] && !rig.valves[4]);
    assert!(!rig.opened_relief_since_failure());
    assert!(rig.trace.iter().filter(|&&c| c == (0, false)).count() > 1, "closure is retried");
    assert!(rig.notes.iter().any(|n| n.contains("Valve acknowledgement timed out in PauseSetup")));
    // Repeated identical retries do not repeat the notification.
    let timeouts = rig.notes.iter().filter(|n| n.contains("timed out")).count();
    assert!(timeouts <= 2, "{:?}", rig.notes);
    rig.stuck[0] = false;
    rig.until(Phase::Paused);
    assert_eq!(rig.valves, CLOSED);
}

#[test]
fn stuck_open_supply_with_climbing_pressure_escalates_to_relief() {
    let mut rig = Rig::new();
    rig.filling();
    rig.stuck[4] = true; // nitrous stays open
    rig.pressure_fresh = false;
    rig.tick();
    rig.pressure_fresh = true;
    for _ in 0..80 {
        rig.pressure += 0.2;
        rig.tick();
        if rig.engine.status.phase == Phase::Fault {
            break;
        }
    }
    assert_eq!(rig.engine.status.phase, Phase::Fault, "{:?}", rig.engine.status);
    assert!(rig.engine.status.message.contains("stuck open while tank pressure climbs"));
    assert!(rig.valves[1] && rig.valves[2]);
}

#[test]
fn stuck_open_supply_without_pressure_rise_keeps_holding_closed() {
    let mut rig = Rig::new();
    rig.filling();
    rig.stuck[4] = true;
    rig.pressure_fresh = false;
    rig.tick();
    rig.pressure_fresh = true;
    for _ in 0..200 {
        rig.tick();
        assert_ne!(rig.engine.status.phase, Phase::Fault);
    }
    assert!(!rig.opened_relief_since_failure());
}

#[test]
fn pressure_ceiling_while_closing_still_relieves() {
    let mut rig = Rig::new();
    rig.filling();
    rig.pressure_fresh = false;
    rig.tick();
    assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
    rig.pressure_fresh = true;
    rig.pressure = 190.0; // limit is min(140 + 50, 200)
    rig.tick();
    assert_eq!(rig.engine.status.phase, Phase::Fault);
    assert!(rig.valves[1] && rig.valves[2]);
}

#[test]
fn pressure_ceiling_during_fill_still_relieves() {
    let mut rig = Rig::new();
    rig.filling();
    rig.pressure = 190.0;
    rig.tick();
    assert_eq!(rig.engine.status.phase, Phase::Fault);
    assert!(rig.valves[1] && rig.valves[2] && !rig.valves[3] && !rig.valves[4]);
}

#[test]
fn nitrogen_step_timeout_pauses_closed_and_can_resume_the_step() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    rig.trace.clear();
    for _ in 0..700 {
        rig.tick();
        if rig.engine.status.phase != Phase::Raising {
            break;
        }
    }
    assert_eq!(rig.engine.status.phase, Phase::PauseSetup);
    rig.until(Phase::Paused);
    assert_eq!(rig.valves, CLOSED);
    assert!(!rig.opened_relief_since_failure());
    assert_eq!(rig.engine.paused_test(), Some(PausedTest::NitrogenTest));
    assert!(!rig.engine.status.nitrogen_passed);
    let effects = rig.engine.resume_test(rig.input()).unwrap();
    rig.apply(effects);
    assert!(rig.engine.failure().is_none());
    rig.until(Phase::Raising);
}

#[test]
fn hold_leak_keeps_its_current_relief() {
    let mut rig = Rig::new();
    rig.request(Action::NitrogenTest);
    rig.until(Phase::Raising);
    rig.pressure = 55.0;
    rig.until(Phase::Holding);
    rig.pressure = 40.0;
    rig.until(Phase::Cancelled);
    assert_eq!(rig.valves, RELIEVED);
}

#[test]
fn already_closed_pause_holds_on_stale_pressure_without_commands() {
    let mut rig = Rig::new();
    rig.filling();
    rig.request(Action::PauseFill);
    rig.until(Phase::Paused);
    rig.trace.clear();
    rig.notes.clear();
    rig.pressure_fresh = false;
    for _ in 0..50 {
        rig.tick();
    }
    assert_eq!(rig.engine.status.phase, Phase::Paused);
    assert!(rig.trace.is_empty());
    assert_eq!(rig.notes.len(), 1, "{:?}", rig.notes);
    assert!(rig.notes[0].starts_with(FAILURE_PAUSE_PREFIX));
    // An operator pause acknowledges it again.
    rig.pressure_fresh = true;
    rig.request(Action::PauseFill);
    assert!(rig.engine.failure().is_none());
}

#[test]
fn operator_cancel_and_existing_fault_are_never_converted_to_a_pause() {
    let mut rig = Rig::new();
    rig.filling();
    rig.request(Action::CancelFill);
    assert!(rig.engine.pause_for_failure(rig.now, "late").valves.is_empty());
    assert_eq!(rig.engine.status.phase, Phase::CancelSetup);
    let _ = rig.engine.fault(rig.now, "hazard");
    assert!(rig.engine.pause_for_failure(rig.now, "late").valves.is_empty());
    assert_eq!(rig.engine.status.phase, Phase::Fault);
}
