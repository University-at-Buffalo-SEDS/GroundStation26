//! Tank overpressure protection: thresholds, debounce, 950 psi ordering,
//! latch/acknowledgement and no interference from T0 onward. Runs in every
//! build (default, hitl_mode, test_fire_mode).
use super::tests::{button_test_state, pump_test_link};
use super::*;

const NITROUS_CLOSE: (&str, u8) = ("ACTUATOR_COMMAND", 13);
const NITROGEN_CLOSE: (&str, u8) = ("ACTUATOR_COMMAND", 12);
const DUMP_OPEN: (&str, u8) = ("VALVE_COMMAND", 2);
const VENT_OPEN: (&str, u8) = ("VALVE_COMMAND", 1);
const PILOT_OPEN: (&str, u8) = ("VALVE_COMMAND", 0);
const PILOT_CLOSE: (&str, u8) = ("VALVE_COMMAND", 3);

struct Fixture {
    state: Arc<AppState>,
    sent: super::tests::TestCommands,
    peer: Arc<sedsnet::router::Router>,
    now: u64,
}
impl Fixture {
    async fn new() -> Self {
        let (state, sent, peer) = button_test_state().await;
        {
            let mut rt = state.gse.lock().unwrap();
            rt.test_now_ms = Some(10_000);
            for key in KEYS {
                rt.observe_valve(key, false);
            }
        }
        Self { state, sent, peer, now: 10_000 }
    }
    /// One new valve-board sample, received and evaluated as on ingress.
    fn sample(&mut self, psi: f32) {
        self.now += 20;
        {
            let mut rt = self.state.gse.lock().unwrap();
            rt.test_now_ms = Some(self.now);
            rt.valve_pressure = Some(Sample { value: psi, at_ms: self.now });
        }
        overpressure::evaluate(&self.state);
        pump_test_link(&self.state, &self.peer);
    }
    fn advance(&mut self, ms: u64) {
        self.now += ms;
        self.state.gse.lock().unwrap().test_now_ms = Some(self.now);
        overpressure::evaluate(&self.state);
        pump_test_link(&self.state, &self.peer);
    }
    fn commands(&self) -> Vec<(String, u8)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, bytes)| (kind.clone(), bytes[0] & 0x7f))
            .collect()
    }
    fn has(&self, command: (&str, u8)) -> bool {
        self.commands().iter().any(|(k, c)| k == command.0 && *c == command.1)
    }
    fn position(&self, command: (&str, u8)) -> Option<usize> {
        self.commands().iter().position(|(k, c)| k == command.0 && *c == command.1)
    }
    fn latch(&self) -> Option<overpressure::Latch> {
        self.state.gse.lock().unwrap().overpressure.latch.clone()
    }
}

fn assert_relief_order(f: &Fixture) {
    let order: Vec<_> = [NITROUS_CLOSE, NITROGEN_CLOSE, DUMP_OPEN, VENT_OPEN]
        .into_iter()
        .map(|c| f.position(c).unwrap_or_else(|| panic!("missing {c:?}: {:?}", f.commands())))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{:?}", f.commands());
}

#[tokio::test]
async fn relief_threshold_closes_supplies_opens_relief_and_latches_in_every_state() {
    for setup in 0..5 {
        let mut f = Fixture::new().await;
        {
            let mut rt = f.state.gse.lock().unwrap();
            match setup {
                0 => {} // idle
                1 => rt.manual_control = true,
                2 => rt.engine.status.phase = Phase::Paused,
                3 => {
                    rt.engine.status.phase = Phase::Filling;
                    rt.ground_station_control = false; // manual fill control
                }
                _ => {
                    let input = rt.input(true, true);
                    let _ = rt.engine.pause_for_failure(input.now_ms, "stale data");
                }
            }
        }
        f.sample(849.0);
        f.sample(849.9);
        assert!(f.latch().is_none() && f.commands().is_empty(), "setup {setup}");
        f.sample(851.0);
        f.sample(860.0);
        assert_relief_order(&f);
        assert!(!f.has(PILOT_OPEN), "no main valve below the abort pressure");
        let latch = f.latch().expect("latched");
        assert_eq!(latch.message, "Tank overpressure: 860 psi > 850");
        assert!(latch.relief_commanded);
        let rt = f.state.gse.lock().unwrap();
        assert_eq!(rt.engine.status.phase, Phase::Fault, "setup {setup}");
        assert!(rt.fill_trim.is_none() && rt.turbocharge.is_none());
        drop(rt);
        assert!(!f.state.abort_indicator_latched());
        assert_eq!(command_allowed(&f.state, &TelemetryCommand::Launch), Some(false));
    }
}

#[tokio::test]
async fn single_spikes_stale_and_repeated_samples_never_trip() {
    let mut f = Fixture::new().await;
    for _ in 0..5 {
        f.sample(990.0);
        f.sample(100.0);
    }
    // The same capture evaluated repeatedly counts once.
    f.sample(990.0);
    for _ in 0..5 {
        overpressure::evaluate(&f.state);
    }
    f.sample(100.0);
    // Stale samples neither trip nor count.
    for _ in 0..3 {
        f.now += 20;
        let mut rt = f.state.gse.lock().unwrap();
        rt.test_now_ms = Some(f.now);
        rt.valve_pressure = Some(Sample { value: 990.0, at_ms: f.now - 2_500 + f.now % 7 });
        drop(rt);
        overpressure::evaluate(&f.state);
    }
    // Calibrated IADC never substitutes for the valve-board lane.
    f.state.gse.lock().unwrap().observe_pressure(Some(2000.0));
    overpressure::evaluate(&f.state);
    pump_test_link(&f.state, &f.peer);
    assert!(f.latch().is_none());
    assert!(f.commands().is_empty(), "{:?}", f.commands());
}

#[tokio::test]
async fn abort_threshold_opens_main_valve_for_the_window_then_closes_it_before_abort() {
    let mut f = Fixture::new().await;
    f.sample(960.0);
    f.sample(965.0);
    assert_relief_order(&f);
    let pilot_open = f.position(PILOT_OPEN).expect("main valve opened");
    assert!(pilot_open > f.position(VENT_OPEN).unwrap(), "relief first, then main valve");
    assert!(!f.state.abort_indicator_latched());
    // Inside the window: no close, no Abort, nothing else commanded.
    let before = f.commands().len();
    f.advance(2_999);
    assert!(!f.has(PILOT_CLOSE));
    assert!(!f.state.abort_indicator_latched(), "Abort must not cut the 3 s window short");
    assert_eq!(f.commands().len(), before);
    // The ordinary tick cannot re-close anything while the latch owns valves.
    tick(&f.state, true);
    pump_test_link(&f.state, &f.peer);
    assert!(!f.has(PILOT_CLOSE));
    f.advance(1);
    assert!(f.has(PILOT_CLOSE), "pilot closed at 3 s: {:?}", f.commands());
    assert!(f.state.abort_indicator_latched(), "Abort dispatched after the pilot close");
    assert_eq!(
        f.latch().unwrap().abort,
        overpressure::AbortStage::Aborted
    );
    // One window, one Abort.
    let after = f.commands().len();
    f.advance(5_000);
    assert_eq!(f.commands().len(), after);
}

#[tokio::test]
async fn relief_then_rising_to_abort_escalates_once() {
    let mut f = Fixture::new().await;
    f.sample(870.0);
    f.sample(880.0);
    assert!(!f.has(PILOT_OPEN));
    f.sample(951.0);
    assert!(!f.has(PILOT_OPEN), "debounced");
    f.sample(955.0);
    assert!(f.has(PILOT_OPEN));
    f.advance(3_000);
    assert!(f.has(PILOT_CLOSE) && f.state.abort_indicator_latched());
}

#[tokio::test]
async fn after_launch_admission_overpressure_only_alarms() {
    let mut f = Fixture::new().await;
    f.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    f.sample(960.0);
    f.sample(990.0);
    f.advance(4_000);
    assert!(f.commands().is_empty(), "no valve action after T0: {:?}", f.commands());
    assert!(!f.state.abort_indicator_latched());
    let latch = f.latch().expect("alarm latched");
    assert!(!latch.relief_commanded);
    assert!(latch.message.contains("main valve is already open for firing"));
    // An alarm-only latch never blocks the pilot or other burn controls.
    assert_ne!(
        overpressure::command_allowed(&f.state.gse.lock().unwrap(), &TelemetryCommand::Pilot),
        Some(false)
    );
}

#[tokio::test]
async fn latched_error_blocks_reclosure_until_acknowledged_below_relief() {
    let mut f = Fixture::new().await;
    f.sample(900.0);
    f.sample(900.0);
    {
        let mut rt = f.state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip([false, true, true, false, false]) {
            rt.observe_valve(key, open);
        }
    }
    let allowed = |cmd| overpressure::command_allowed(&f.state.gse.lock().unwrap(), &cmd);
    assert_eq!(allowed(TelemetryCommand::Dump), Some(false), "cannot close dump");
    assert_eq!(allowed(TelemetryCommand::NormallyOpen), Some(false), "cannot close vent");
    assert_eq!(allowed(TelemetryCommand::Nitrous), Some(false), "cannot open nitrous");
    assert_eq!(allowed(TelemetryCommand::Nitrogen), Some(false));
    assert_eq!(allowed(TelemetryCommand::NitrousClose), Some(true));
    assert_eq!(allowed(TelemetryCommand::StartFill), Some(false));
    assert_eq!(allowed(TelemetryCommand::PauseFill), Some(false));
    assert_eq!(allowed(TelemetryCommand::Turbocharge), Some(false));
    assert_eq!(allowed(TelemetryCommand::Abort), Some(true));
    // Still high: acknowledgement refused.
    assert!(overpressure::acknowledge(&f.state).is_err());
    let before = f.commands().len();
    for _ in 0..20 {
        f.sample(400.0);
        tick(&f.state, true);
        pump_test_link(&f.state, &f.peer);
    }
    assert_eq!(f.commands().len(), before, "never re-closed automatically");
    assert!(f.latch().is_some(), "stays latched after pressure drops");
    // Stale reading cannot acknowledge.
    f.now += 3_000;
    f.state.gse.lock().unwrap().test_now_ms = Some(f.now);
    assert!(overpressure::acknowledge(&f.state).is_err());
    f.sample(400.0);
    overpressure::acknowledge(&f.state).unwrap();
    assert!(f.latch().is_none());
    assert_eq!(overpressure::command_allowed(&f.state.gse.lock().unwrap(), &TelemetryCommand::Dump), None);
    assert_eq!(f.commands().len(), before, "acknowledgement commands nothing");
}

#[tokio::test]
async fn acknowledgement_is_refused_during_the_main_valve_window() {
    let mut f = Fixture::new().await;
    f.sample(960.0);
    f.sample(960.0);
    f.sample(100.0);
    assert!(overpressure::acknowledge(&f.state).is_err());
    f.advance(3_000);
    f.sample(100.0);
    overpressure::acknowledge(&f.state).unwrap();
}

#[tokio::test]
async fn invalid_threshold_settings_fall_back_to_the_defaults() {
    let mut f = Fixture::new().await;
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.engine.config.overpressure_relief_psi = 2_000.0;
        rt.engine.config.overpressure_abort_psi = 1_000.0;
    }
    f.sample(860.0);
    f.sample(860.0);
    assert!(f.latch().is_some());
}

#[tokio::test]
async fn configured_thresholds_and_window_are_used() {
    let mut f = Fixture::new().await;
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.engine.config.overpressure_relief_psi = 700.0;
        rt.engine.config.overpressure_abort_psi = 800.0;
        rt.engine.config.overpressure_pilot_ms = 1_000;
    }
    f.sample(805.0);
    f.sample(805.0);
    assert!(f.has(PILOT_OPEN));
    f.advance(980);
    assert!(!f.has(PILOT_CLOSE));
    f.advance(20);
    assert!(f.has(PILOT_CLOSE));
}

#[test]
fn older_clients_saving_settings_keep_the_overpressure_thresholds() {
    let current = Config {
        overpressure_relief_psi: 800.0,
        overpressure_abort_psi: 900.0,
        overpressure_pilot_ms: 2_000,
        tank_rating_psi: Some(1_000.0),
        ..Config::default()
    };
    let mut legacy = serde_json::to_value(Config::default()).unwrap();
    for key in OVERPRESSURE_CONFIG_KEYS {
        legacy.as_object_mut().unwrap().remove(key);
    }
    let saved = config_from_request(&current, legacy).unwrap();
    assert_eq!(saved.overpressure_relief_psi, 800.0);
    assert_eq!(saved.overpressure_abort_psi, 900.0);
    assert_eq!(saved.overpressure_pilot_ms, 2_000);
    assert_eq!(saved.tank_rating_psi, Some(1_000.0));
    let explicit = serde_json::to_value(Config::default()).unwrap();
    assert_eq!(config_from_request(&current, explicit).unwrap().overpressure_relief_psi, 850.0);
    assert!(config_from_request(&current, serde_json::json!({"unknown": 1})).is_err());
}
