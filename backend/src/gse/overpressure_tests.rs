//! Tank overpressure protection: thresholds, sustained time, implausible
//! sensor steps, 950 psi ordering,
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
const AB_ABORT: (&str, u8) = ("ACTUATOR_COMMAND", 16);

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
    /// 13 samples at 50 Hz span 240 ms, one short of the 250 ms sustained
    /// time: the next sample at or above the threshold is the first that trips.
    fn lead(&mut self, psi: f32) {
        for _ in 0..13 {
            self.sample(psi);
        }
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
        f.lead(849.9);
        f.sample(849.9);
        assert!(f.latch().is_none() && f.commands().is_empty(), "setup {setup}");
        f.lead(851.0);
        assert!(f.latch().is_none() && f.commands().is_empty(), "setup {setup}: 240 ms");
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
    f.lead(960.0);
    assert!(f.commands().is_empty());
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
    f.lead(870.0);
    f.sample(880.0);
    assert!(f.latch().is_some() && !f.has(PILOT_OPEN));
    f.sample(951.0);
    for _ in 0..12 {
        f.sample(952.0);
        assert!(!f.has(PILOT_OPEN), "abort pressure needs its own 250 ms");
    }
    f.sample(955.0);
    assert!(f.has(PILOT_OPEN));
    f.advance(3_000);
    assert!(f.has(PILOT_CLOSE) && f.state.abort_indicator_latched());
}

#[tokio::test]
async fn after_launch_admission_overpressure_only_alarms() {
    let mut f = Fixture::new().await;
    f.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    f.lead(960.0);
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
    f.lead(900.0);
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
    f.lead(960.0);
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
    f.lead(860.0);
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
    f.lead(805.0);
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

const BOARD_ACK: (&str, u8) = ("VALVE_COMMAND", overpressure::BOARD_ACK_COMMAND);

impl Fixture {
    fn board(&mut self, text: &str) -> Option<String> {
        let shown = overpressure::board_warning(&self.state, text);
        pump_test_link(&self.state, &self.peer);
        shown
    }
    fn allowed(&self, cmd: TelemetryCommand) -> Option<bool> {
        overpressure::command_allowed(&self.state.gse.lock().unwrap(), &cmd)
    }
}

#[test]
fn valve_board_reports_parse_only_their_exact_tag() {
    use overpressure::parse_board_report as parse;
    assert_eq!(parse("Valve Board overpressure relief: 912 psi [R3]"), Some(('R', 912, 3)));
    assert_eq!(parse("Valve Board overpressure relief, main valve open: 955 psi [P4]"), Some(('P', 955, 4)));
    assert_eq!(
        parse("Valve Board overpressure relief, main valve closed, Abort: 970 psi [A1]"),
        Some(('A', 970, 1))
    );
    assert_eq!(parse("Valve Board overpressure during launch, no valve action: 990 psi [L1]"), Some(('L', 990, 1)));
    assert_eq!(parse("Valve Board overpressure acknowledged: 870 psi [C1]"), Some(('C', 870, 1)));
    assert_eq!(parse("Valve Board overpressure sensor invalid, raw ADC 4095 [F0]"), Some(('F', 4095, 0)));
    assert_eq!(parse("Valve Board overpressure sensor stale [S2]"), Some(('S', 0, 2)));
    assert_eq!(
        parse("Valve Board overpressure sensor implausible or disconnected: 1038 psi [I0]"),
        Some(('I', 1038, 0))
    );
    for text in [
        "Valve Board overpressure relief: 912 psi [X3]",
        "Valve Board overpressure relief: 912 psi [R]",
        "Valve Board overpressure relief: 912 psi",
        "Valve Board overpressure relief: psi [R3]",
        "Tank overpressure: 912 psi [R3]",
        "",
    ] {
        assert_eq!(parse(text), None, "{text}");
    }
}

#[tokio::test]
async fn stale_sensor_reports_stay_quiet_while_the_valve_board_is_offline_or_reconnecting() {
    let mut f = Fixture::new().await;
    let stale = "Valve Board overpressure sensor stale [S2]";
    let observe = |f: &Fixture, at: std::time::Instant| {
        f.state.network_health.lock().unwrap().observe("VB", at, 0);
    };
    // Offline: the board-offline alert already covers it.
    assert!(f.board(stale).is_none());
    // Just reconnected: the report describes the outage, not the sensor.
    observe(&f, std::time::Instant::now());
    assert!(f.board(stale).is_none());
    // Connected well past the grace: a stale sensor is raised.
    let now = std::time::Instant::now();
    let mut f2 = Fixture::new().await;
    observe(&f2, now - std::time::Duration::from_secs(11));
    observe(&f2, now);
    assert_eq!(f2.board(stale).as_deref(), Some("Valve Board overpressure sensor stale"));
    // While the Valve Board applies a recover or resync it drives its outputs
    // safe and can miss samples: that stale report is not raised either.
    let mut f3 = Fixture::new().await;
    observe(&f3, now - std::time::Duration::from_secs(11));
    observe(&f3, now);
    f3.state.board_recovery.note_apply_requested(crate::types::Board::ValveBoard);
    assert!(f3.board(stale).is_none());
    // Invalid and implausible sensors are real faults and always warn.
    assert!(f.board("Valve Board overpressure sensor invalid: 4095 [F1]").is_some());
}

#[tokio::test]
async fn valve_board_relief_is_adopted_once_and_blocks_reclosure() {
    let mut f = Fixture::new().await;
    // Ordinary warnings pass through untouched.
    assert_eq!(f.board("Low battery").as_deref(), Some("Low battery"));
    assert!(f.board("Valve Board overpressure relief: 912 psi [R3]").is_none());
    assert_relief_order(&f);
    assert!(!f.has(PILOT_OPEN), "the board owns the main valve");
    let latch = f.latch().expect("adopted");
    assert!(latch.relief_commanded && latch.abort == overpressure::AbortStage::NotRequired);
    assert_eq!(latch.message, "Valve Board overpressure relief: 912 psi");
    assert_eq!(f.state.gse.lock().unwrap().engine.status.phase, Phase::Fault);
    let sent = f.commands().len();
    // Re-announcements are silent and command nothing new.
    for _ in 0..5 {
        f.advance(1_000);
        assert!(f.board("Valve Board overpressure relief: 912 psi [R3]").is_none());
    }
    assert_eq!(f.commands().len(), sent);
    {
        let mut rt = f.state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip([false, true, true, false, false]) {
            rt.observe_valve(key, open);
        }
    }
    assert_eq!(f.allowed(TelemetryCommand::Dump), Some(false));
    assert_eq!(f.allowed(TelemetryCommand::NormallyOpen), Some(false));
    assert_eq!(f.allowed(TelemetryCommand::Nitrous), Some(false));
    assert_eq!(f.allowed(TelemetryCommand::Launch), Some(false));
    assert_eq!(f.allowed(TelemetryCommand::Abort), Some(true));
}

#[tokio::test]
async fn station_acknowledgement_also_acknowledges_the_board() {
    let mut f = Fixture::new().await;
    f.board("Valve Board overpressure relief: 900 psi [R1]");
    f.sample(400.0);
    overpressure::acknowledge(&f.state).unwrap();
    assert!(f.latch().is_none());
    pump_test_link(&f.state, &f.peer);
    assert!(f.has(BOARD_ACK), "{:?}", f.commands());
    // A report already in flight does not relatch, but the board latch still
    // refuses re-closing dump and vent until the board confirms.
    assert!(f.board("Valve Board overpressure relief: 900 psi [R1]").is_none());
    assert!(f.latch().is_none());
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.observe_valve(KEYS[2], true);
    }
    assert_eq!(f.allowed(TelemetryCommand::Dump), Some(false));
    // Board confirmation releases the station.
    assert_eq!(
        f.board("Valve Board overpressure acknowledged: 900 psi [C1]").as_deref(),
        Some("Valve Board overpressure acknowledged: 900 psi")
    );
    assert_eq!(f.allowed(TelemetryCommand::Dump), None);
}

#[tokio::test]
async fn board_refusing_acknowledgement_is_adopted_again() {
    let mut f = Fixture::new().await;
    f.board("Valve Board overpressure relief: 900 psi [R1]");
    f.sample(400.0);
    overpressure::acknowledge(&f.state).unwrap();
    f.advance(3_000);
    f.board("Valve Board overpressure relief: 900 psi [R1]");
    assert!(f.latch().is_some_and(|l| l.relief_commanded));
}

#[tokio::test]
async fn board_latch_alone_can_be_acknowledged_and_expires_when_silent() {
    let mut f = Fixture::new().await;
    f.board("Valve Board overpressure relief: 900 psi [R1]");
    f.state.gse.lock().unwrap().overpressure.latch = None; // station restarted
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.observe_valve(KEYS[1], true);
    }
    assert_eq!(f.allowed(TelemetryCommand::NormallyOpen), Some(false));
    f.sample(870.0);
    assert!(overpressure::acknowledge(&f.state).is_err(), "still high");
    f.advance(300);
    f.sample(400.0);
    overpressure::acknowledge(&f.state).unwrap();
    pump_test_link(&f.state, &f.peer);
    assert!(f.has(BOARD_ACK));
    // No longer announced (board reset or link loss): the station stops refusing.
    f.advance(5_001);
    assert_eq!(f.allowed(TelemetryCommand::NormallyOpen), None);
}

#[tokio::test]
async fn board_main_valve_window_blocks_acknowledgement_and_station_window() {
    let mut f = Fixture::new().await;
    assert!(f.board("Valve Board overpressure relief, main valve open: 960 psi [P1]").is_none());
    let latch = f.latch().unwrap();
    assert_eq!(latch.abort, overpressure::AbortStage::Aborted, "board owns the window and Abort");
    f.lead(960.0);
    f.sample(960.0);
    assert!(!f.has(PILOT_OPEN), "no second, station-timed window");
    f.advance(300);
    f.sample(100.0);
    assert!(overpressure::acknowledge(&f.state).is_err());
    f.board("Valve Board overpressure relief, main valve closed, Abort: 960 psi [A1]");
    f.sample(100.0);
    overpressure::acknowledge(&f.state).unwrap();
}

#[tokio::test]
async fn board_launch_alarm_and_sensor_faults_command_nothing() {
    let mut f = Fixture::new().await;
    assert!(f.board("Valve Board overpressure during launch, no valve action: 990 psi [L1]").is_none());
    assert!(f.commands().is_empty());
    assert!(f.latch().is_some_and(|l| !l.relief_commanded));
    assert_eq!(f.allowed(TelemetryCommand::Dump), None);
    let mut g = Fixture::new().await;
    assert_eq!(
        g.board("Valve Board overpressure sensor invalid, raw ADC 4095 [F0]").as_deref(),
        Some("Valve Board overpressure sensor invalid, raw ADC 4095")
    );
    assert!(g.commands().is_empty() && g.latch().is_none());
}

impl Fixture {
    /// Raw wire payloads of every Actuator Board overpressure abort sent.
    fn ab_aborts(&self) -> usize {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, bytes)| kind == "ACTUATOR_COMMAND" && bytes[..] == [0x90])
            .count()
    }
}

#[tokio::test]
async fn relief_threshold_sends_the_actuator_board_overpressure_abort_once() {
    let mut f = Fixture::new().await;
    f.lead(851.0);
    assert_eq!(f.ab_aborts(), 0, "not yet 250 ms");
    f.sample(860.0);
    assert_eq!(f.ab_aborts(), 1, "AB abort [0x90] at relief: {:?}", f.sent.lock().unwrap());
    assert_relief_order(&f);
    assert!(f.position(AB_ABORT).unwrap() > f.position(VENT_OPEN).unwrap());
    assert!(!f.has(PILOT_OPEN));
    assert!(!f.state.abort_indicator_latched(), "relief alone never broadcasts Abort");
    assert!(f.latch().unwrap().ab_abort_sent);
    // Re-evaluation, more samples and the escalation to the abort stage do
    // not repeat it; the broadcast Abort still waits for the pilot window.
    f.sample(870.0);
    f.advance(1_000);
    f.lead(955.0);
    f.sample(960.0);
    assert!(f.has(PILOT_OPEN));
    assert_eq!(f.ab_aborts(), 1);
    f.advance(3_000);
    assert!(f.has(PILOT_CLOSE) && f.state.abort_indicator_latched());
    assert_eq!(f.ab_aborts(), 1);
}

#[tokio::test]
async fn abort_threshold_sends_the_actuator_board_abort_before_the_pilot_window_ends() {
    let mut f = Fixture::new().await;
    f.lead(960.0);
    f.sample(965.0);
    // Sent with the trip, while the main valve window is still running and
    // before the pilot close and the broadcast Abort.
    assert_eq!(f.ab_aborts(), 1, "{:?}", f.sent.lock().unwrap());
    assert!(f.has(PILOT_OPEN) && !f.has(PILOT_CLOSE));
    assert!(!f.state.abort_indicator_latched());
    assert!(matches!(f.latch().unwrap().abort, overpressure::AbortStage::PilotOpen { .. }));
    f.advance(2_999);
    assert!(!f.has(PILOT_CLOSE) && !f.state.abort_indicator_latched());
    f.advance(1);
    let ab = f.position(AB_ABORT).unwrap();
    assert!(ab < f.position(PILOT_CLOSE).unwrap());
    assert!(f.state.abort_indicator_latched(), "broadcast Abort after the window");
    assert_eq!(f.ab_aborts(), 1);
}

#[tokio::test]
async fn abort_stage_retries_an_actuator_board_abort_the_relief_stage_did_not_send() {
    let mut f = Fixture::new().await;
    f.lead(860.0);
    f.sample(860.0);
    // As if the relief-stage send had failed.
    f.state.gse.lock().unwrap().overpressure.latch.as_mut().unwrap().ab_abort_sent = false;
    let before = f.ab_aborts();
    f.lead(955.0);
    f.sample(955.0);
    assert!(f.has(PILOT_OPEN) && !f.has(PILOT_CLOSE));
    assert_eq!(f.ab_aborts(), before + 1, "sent at once at 950");
    assert!(!f.state.abort_indicator_latched());
}

#[tokio::test]
async fn no_actuator_board_abort_after_launch_admission() {
    let mut f = Fixture::new().await;
    f.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    f.lead(870.0);
    f.sample(880.0);
    f.lead(960.0);
    f.sample(990.0);
    f.advance(4_000);
    assert_eq!(f.ab_aborts(), 0);
    assert!(f.commands().is_empty(), "{:?}", f.commands());
    // A Valve Board report adopted after Launch admission only alarms too.
    let mut g = Fixture::new().await;
    g.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
    assert!(g.board("Valve Board overpressure relief: 912 psi [R3]").is_none());
    assert_eq!(g.ab_aborts(), 0);
    assert!(g.commands().is_empty(), "{:?}", g.commands());
}

#[tokio::test]
async fn adopted_valve_board_relief_also_sends_the_actuator_board_abort() {
    let mut f = Fixture::new().await;
    assert!(f.board("Valve Board overpressure relief: 912 psi [R3]").is_none());
    assert_eq!(f.ab_aborts(), 1);
    for _ in 0..3 {
        f.advance(1_000);
        f.board("Valve Board overpressure relief: 912 psi [R3]");
    }
    assert_eq!(f.ab_aborts(), 1, "re-announcements send nothing new");
    let mut g = Fixture::new().await;
    g.board("Valve Board overpressure during launch, no valve action: 990 psi [L1]");
    assert_eq!(g.ab_aborts(), 0);
}

#[tokio::test]
async fn actuator_board_abort_passes_a_pending_stop_like_the_supply_closes() {
    let mut f = Fixture::new().await;
    f.state.set_abort_indicator_latched(true);
    f.lead(860.0);
    f.sample(860.0);
    assert!(f.has(NITROUS_CLOSE) && f.has(NITROGEN_CLOSE));
    assert_eq!(f.ab_aborts(), 1);
}

impl Fixture {
    /// One valve-board sample `dt` ms after the previous, evaluated without
    /// pumping the link (long traces).
    fn quiet_sample(&mut self, psi: f32, dt: u64) {
        self.now += dt;
        {
            let mut rt = self.state.gse.lock().unwrap();
            rt.test_now_ms = Some(self.now);
            rt.valve_pressure = Some(Sample { value: psi, at_ms: self.now });
        }
        overpressure::evaluate(&self.state);
    }
    fn status(&self) -> serde_json::Value {
        let rt = self.state.gse.lock().unwrap();
        let mut response = serde_json::json!({});
        overpressure::decorate_status(&rt, true, &mut response);
        response
    }
}

/// Recorded live behaviour with no PT connected to the Valve Board: about
/// 50 Hz (19-23 ms), mostly 0.0 psi, wandering up to 94 psi, plus 2-3 sample
/// spikes to about 1038 psi when the main valve solenoid switches, for
/// 8 minutes. Nothing trips, before or after Launch; the operator sees a
/// sensor fault instead.
#[tokio::test]
async fn floating_input_trace_never_trips_and_reports_an_implausible_sensor() {
    for launched in [false, true] {
        let mut f = Fixture::new().await;
        if launched {
            f.state.launch_history.lock().unwrap().inhibit_automatic_actions().unwrap();
        }
        let mut rng: u32 = 0x5eed_1234;
        let mut next = |n: u32| {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (rng >> 8) % n
        };
        let mut level = 0.0f32;
        let mut spikes = 0;
        let start = f.now;
        while f.now - start < 8 * 60 * 1000 {
            let dt = 19 + next(5) as u64;
            if next(200) == 0 {
                spikes += 1;
                for _ in 0..2 + next(2) {
                    f.quiet_sample(1000.0 + next(39) as f32, dt);
                }
                continue;
            }
            if next(4) == 0 {
                level = next(95) as f32;
            } else if next(3) == 0 {
                level = 0.0;
            }
            f.quiet_sample(level, dt);
            assert!(f.latch().is_none(), "tripped on the floating input");
        }
        pump_test_link(&f.state, &f.peer);
        assert!(spikes > 100);
        assert!(f.latch().is_none() && f.commands().is_empty(), "{:?}", f.commands());
        assert!(!f.state.abort_indicator_latched());
        let fault = f.state.gse.lock().unwrap().overpressure.sensor_fault.clone().expect("fault");
        assert!(fault.count as usize >= spikes, "every spike is an implausible step");
        // Visible in /api/gse/status for a minute after the last glitch.
        let status = f.status();
        assert_eq!(status["overpressure_latched"], false);
        assert_eq!(status["overpressure"]["sensor"]["fault"], true, "{status}");
        assert_eq!(status["overpressure"]["sensor"]["implausible"], true);
        let message = status["overpressure"]["sensor"]["message"].as_str().unwrap();
        assert!(message.starts_with("Tank pressure sensor implausible or not connected"), "{message}");
        f.advance(61_000);
        assert_eq!(f.status()["overpressure"]["sensor"]["fault"], false);
    }
}

#[tokio::test]
async fn implausible_step_resets_timers_settles_and_then_still_protects() {
    let mut f = Fixture::new().await;
    // Exactly the plausible limit passes; one psi more is a fault, both ways.
    f.sample(100.0);
    f.sample(250.0);
    f.sample(100.0);
    assert!(!f.state.gse.lock().unwrap().overpressure.settling());
    f.sample(251.0);
    assert!(f.state.gse.lock().unwrap().overpressure.settling());
    // The limit scales with the time between readings.
    let mut g = Fixture::new().await;
    g.sample(100.0);
    g.quiet_sample(400.0, 40);
    assert!(!g.state.gse.lock().unwrap().overpressure.settling());
    g.quiet_sample(701.0, 40);
    assert!(g.state.gse.lock().unwrap().overpressure.settling());

    // The incident: 0 -> 1038 psi that then sits high. No trip while settling.
    let mut f = Fixture::new().await;
    for _ in 0..50 {
        f.sample(0.0);
    }
    // 50 samples span 980 ms of settling; the 51st (1000 ms) counts again.
    for _ in 0..51 {
        f.sample(1038.0);
        assert!(f.latch().is_none() && f.commands().is_empty());
    }
    assert!(!f.state.gse.lock().unwrap().overpressure.settling(), "settled 1000 ms after the step");
    assert_eq!(f.status()["overpressure"]["sensor"]["settling"], false);
    // A genuine high reading is delayed by settle time + sustain, never lost.
    for _ in 0..12 {
        f.sample(1038.0);
        assert!(f.latch().is_none());
    }
    f.sample(1038.0);
    assert!(f.has(PILOT_OPEN) && f.latch().is_some());

    // Each further implausible step restarts settling, and settling readings
    // are not acknowledgement evidence.
    let mut f = Fixture::new().await;
    f.lead(900.0);
    f.sample(900.0);
    assert!(f.latch().is_some());
    for psi in [800.0, 700.0, 600.0, 500.0, 400.0, 300.0, 200.0, 100.0] {
        f.sample(psi);
    }
    f.sample(700.0);
    f.sample(100.0);
    let refused = overpressure::acknowledge(&f.state).unwrap_err();
    assert!(refused.contains("implausible"), "{refused}");
    assert_eq!(f.status()["overpressure"]["sensor"]["settling"], true);
    for _ in 0..50 {
        f.sample(100.0);
    }
    overpressure::acknowledge(&f.state).unwrap();
}

/// A genuine tank ramp from 100 to 1000 psi over seconds still trips relief
/// at 850 psi and the main-valve window plus Abort at 950 psi.
#[tokio::test]
async fn genuine_ramp_trips_relief_at_850_then_main_valve_and_abort_at_950() {
    for (ramp_ms, dt) in [(2_000u64, 20u64), (3_000, 21), (10_000, 33)] {
        let mut f = Fixture::new().await;
        let start = f.now;
        let (mut at850, mut at950, mut relief, mut pilot) = (None, None, None, None);
        while pilot.is_none() {
            let t = f.now + dt - start;
            let psi = if t >= ramp_ms { 1000.0 } else { 100.0 + 900.0 * t as f32 / ramp_ms as f32 };
            f.now += dt;
            {
                let mut rt = f.state.gse.lock().unwrap();
                rt.test_now_ms = Some(f.now);
                rt.valve_pressure = Some(Sample { value: psi, at_ms: f.now });
            }
            overpressure::evaluate(&f.state);
            pump_test_link(&f.state, &f.peer);
            if psi >= 850.0 && at850.is_none() {
                at850 = Some(f.now);
            }
            if psi >= 950.0 && at950.is_none() {
                at950 = Some(f.now);
            }
            if relief.is_none() && f.has(DUMP_OPEN) {
                relief = Some(f.now);
            }
            if f.has(PILOT_OPEN) {
                pilot = Some(f.now);
            }
            assert!(f.now - start < ramp_ms + 5_000);
        }
        let (at850, at950, relief, pilot) = (at850.unwrap(), at950.unwrap(), relief.unwrap(), pilot.unwrap());
        assert!(relief >= at850 + SUSTAIN && relief < at850 + SUSTAIN + dt, "{ramp_ms}: {relief} vs {at850}");
        assert!(pilot >= at950 + SUSTAIN && pilot < at950 + SUSTAIN + dt, "{ramp_ms}: {pilot} vs {at950}");
        assert_relief_order(&f);
        assert!(f.state.gse.lock().unwrap().overpressure.sensor_fault.is_none());
        f.advance(2_999);
        assert!(!f.has(PILOT_CLOSE) && !f.state.abort_indicator_latched());
        f.advance(1);
        assert!(f.has(PILOT_CLOSE) && f.state.abort_indicator_latched(), "{ramp_ms}");
    }
}
const SUSTAIN: u64 = overpressure::SUSTAIN_MS;

#[tokio::test]
async fn board_implausible_sensor_report_is_shown_once_and_never_adopted() {
    let mut f = Fixture::new().await;
    let text = "Valve Board overpressure sensor implausible or disconnected: 1038 psi [I0]";
    let shown = f.board(text).expect("shown");
    assert!(shown.contains("not connected") && shown.contains("not an overpressure"), "{shown}");
    assert!(f.board(text).is_none(), "repeat within a minute is silent");
    assert!(f.commands().is_empty() && f.latch().is_none());
    assert_eq!(f.allowed(TelemetryCommand::Dump), None);
    let status = f.status();
    assert_eq!(status["overpressure"]["sensor"]["implausible"], true, "{status}");
    assert_eq!(status["overpressure"]["sensor"]["valve_board"]["code"], "I");
    assert_eq!(status["overpressure"]["sensor"]["valve_board"]["value"], 1038);
}
