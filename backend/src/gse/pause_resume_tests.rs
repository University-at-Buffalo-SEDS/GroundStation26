//! Pause, resume and cancel of every engine-owned sequence over the routed
//! SDK command path. Valve reports follow the commands the peer actually
//! received; a simple tank model supplies pressure and mass.
use super::*;
use crate::sequence_progress::Outcome;
use super::tests::{TestCommands, button_test_state, pump_test_link};

struct Plant {
    state: Arc<AppState>,
    sent: TestCommands,
    peer: Arc<sedsnet::router::Router>,
    valves: [bool; 5],
    consumed: usize,
    elapsed: u64,
    pressure: f32,
    mass: f32,
    /// Closed-tank pressure loss per 100 ms step (a leak).
    leak: f32,
    #[cfg(not(feature = "hitl_mode"))]
    path: std::path::PathBuf,
}
impl Drop for Plant {
    fn drop(&mut self) {
        #[cfg(not(feature = "hitl_mode"))]
        let _ = std::fs::remove_file(&self.path);
    }
}
impl Plant {
    async fn new() -> Self {
        let (state, sent, peer) = button_test_state().await;
        #[cfg(not(feature = "hitl_mode"))]
        let path = std::env::temp_dir().join(format!("gse-pause-{}.json", rand::random::<u128>()));
        let plant = Self {
            state,
            sent,
            peer,
            valves: gse_sequence::RELIEVED,
            consumed: 0,
            elapsed: 3000,
            pressure: 0.0,
            mass: 0.0,
            leak: 0.0,
            #[cfg(not(feature = "hitl_mode"))]
            path,
        };
        plant.refresh_boards();
        let mut targets = plant.state.fill_targets_snapshot();
        targets.nitrous.target_mass_kg = 8.0;
        targets.nitrous.target_pressure_psi = 750.0;
        plant.state.set_fill_targets(targets);
        {
            let mut rt = plant.state.gse.lock().unwrap();
            rt.test_now_ms = Some(plant.elapsed);
            rt.engine.config.dry_self_test_confirmed = true;
            rt.engine.config.nitrogen_target_psi = 120.0;
            #[cfg(not(feature = "hitl_mode"))]
            {
                let (_, key) = operational_policy_topology_key(&plant.state);
                rt.operational.path = Some(plant.path.clone());
                rt.operational.topology(0, true, key.clone());
                rt.operational.topology(3000, true, key);
                rt.operational.startup_configured = true;
            }
        }
        plant.interlock();
        plant.observe();
        plant
    }
    fn interlock(&self) {
        let mut policy = self.state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        self.state.set_action_policy(policy);
    }
    fn refresh_boards(&self) {
        for board in [
            crate::types::Board::GatewayBoard,
            crate::types::Board::ValveBoard,
            crate::types::Board::ActuatorBoard,
            crate::types::Board::DaqBoard,
        ] {
            self.state.mark_board_seen(
                board.sender_id(),
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
    }
    fn observe(&self) {
        let mut rt = self.state.gse.lock().unwrap();
        for (key, open) in KEYS.into_iter().zip(self.valves) {
            rt.observe_valve(key, open);
        }
        rt.observe_pressure(Some(self.pressure));
        rt.observe_mass(Some(self.mass));
    }
    /// Commands the peer received since the last call, as (valve, open).
    fn receive(&mut self) -> Vec<(usize, bool)> {
        pump_test_link(&self.state, &self.peer);
        let commands = self.sent.lock().unwrap();
        let mut received = Vec::new();
        for (kind, bytes) in &commands[self.consumed..] {
            assert_eq!(bytes.len(), 1);
            assert_ne!(bytes[0] & 0x80, 0, "automatic provenance survives the wire");
            let command = bytes[0] & 0x7f;
            let candidates = if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 };
            let index = candidates
                .into_iter()
                .find(|&i| command == OPEN[i] || command == CLOSE[i])
                .expect("only GSE valves are actuated");
            self.valves[index] = command == OPEN[index];
            received.push((index, self.valves[index]));
        }
        self.consumed = commands.len();
        received
    }
    fn step(&mut self) -> Vec<(usize, bool)> {
        let received = self.receive();
        self.elapsed += 100;
        self.refresh_boards();
        if self.valves[1] || self.valves[2] {
            self.pressure = (self.pressure - 20.0).max(0.0);
        } else if self.valves[3] {
            self.pressure += 4.0;
        } else {
            self.pressure = (self.pressure - self.leak).max(0.0);
        }
        if self.valves[4] {
            let target = self.state.fill_targets_snapshot().nitrous.target_mass_kg;
            let config = self.state.gse.lock().unwrap().engine.config.clone();
            self.mass = (self.mass + 0.5).min(target + config.fill_overfill_kg);
        }
        self.state.gse.lock().unwrap().test_now_ms = Some(self.elapsed);
        self.observe();
        tick(&self.state, true);
        // The operator's Dump choice at a passed nitrogen-test hold.
        let hold = {
            let rt = self.state.gse.lock().unwrap();
            rt.engine.status.phase == Phase::Paused
                && rt.engine.test_hold() == Some(&gse_sequence::TestHold::Passed)
        };
        if hold {
            let _dispatch = DISPATCH_GATE.lock().unwrap();
            dump_test_hold_under_dispatch(&self.state).unwrap();
        }
        let rt = self.state.gse.lock().unwrap();
        assert_ne!(rt.engine.status.phase, Phase::Fault, "{}", rt.engine.status.message);
        received
    }
    /// Steps until `done`, returning every command received on the way.
    fn until(&mut self, done: impl Fn(&Runtime) -> bool) -> Vec<(usize, bool)> {
        let mut received = Vec::new();
        for _ in 0..3000 {
            if done(&self.state.gse.lock().unwrap()) {
                return received;
            }
            received.extend(self.step());
        }
        let rt = self.state.gse.lock().unwrap();
        panic!("plant did not reach the condition: {:?} {}", rt.engine.status.phase, rt.engine.status.message);
    }
    fn phase(&self) -> Phase {
        self.state.gse.lock().unwrap().engine.status.phase
    }
    fn outcome(&self) -> Option<Outcome> {
        progress::flush(&self.state);
        self.state.gse.lock().unwrap().progress.current().map(|s| s.outcome)
    }
    fn controls(&self) -> crate::sequence_progress::RunControls {
        progress::controls(&self.state).expect("current run controls")
    }
    fn key(&self) -> crate::sequence_progress::RunKey {
        self.state.gse.lock().unwrap().progress.current().unwrap().key.clone()
    }
    /// The same path the pause endpoint takes after HTTP authorization.
    fn pause(&mut self) -> Vec<(usize, bool)> {
        let controls = self.controls();
        assert!(controls.pause.allowed, "{:?}", controls.pause);
        assert!(!controls.resume.allowed);
        // Deliver whatever the sequence queued before the operator pressed Pause.
        self.receive();
        assert!(handle_command(&self.state, &TelemetryCommand::PauseFill));
        assert_eq!(self.phase(), Phase::PauseSetup);
        let received = self.until(|rt| rt.engine.status.phase == Phase::Paused);
        // One more step delivers fresh closed reports for the confirmation.
        self.step();
        assert_eq!(self.valves, gse_sequence::CLOSED);
        assert!(received.iter().all(|(_, open)| !open), "pause never opens: {received:?}");
        assert!(
            received.first().is_some_and(|(i, _)| *i >= 3),
            "supplies close first: {received:?}"
        );
        assert_eq!(self.outcome(), Some(Outcome::Paused));
        received
    }
    fn resume(&mut self) {
        let controls = self.controls();
        assert!(controls.resume.allowed, "{:?}", controls.resume);
        assert!(!controls.pause.allowed);
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        let kind = self.state.gse.lock().unwrap().progress.current().unwrap().kind;
        if kind == crate::sequence_progress::Kind::Fill {
            handle_command_inner(&self.state, &TelemetryCommand::StartFill, None, None);
        } else {
            resume_test_under_dispatch(&self.state).unwrap();
        }
    }
}
#[cfg(not(feature = "hitl_mode"))]
fn operational_policy_topology_key(state: &AppState) -> (bool, String) {
    let Some(router) = state.topology_router.get() else {
        return (false, String::new());
    };
    let snapshot = router.export_topology();
    let mut keys: Vec<_> = snapshot
        .routes
        .iter()
        .filter(|side| {
            ["VALVE_BOARD", "ACTUATOR_BOARD"].iter().any(|name| {
                side.reachable_endpoints
                    .contains(&crate::telemetry_schema::endpoint(name))
            })
        })
        .map(|side| {
            let mut endpoints: Vec<_> = side
                .reachable_endpoints
                .iter()
                .map(|e| e.as_str().to_string())
                .collect();
            endpoints.sort();
            format!("{}:{endpoints:?}", side.side_id)
        })
        .collect();
    keys.sort();
    (true, keys.join("|"))
}

#[tokio::test]
async fn routed_self_test_nitrogen_and_fill_pause_resume_then_complete() {
    let mut plant = Plant::new().await;
    // Valve self-test: pause on the dump valve's dwell, resume there.
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::SelfHold && rt.engine.progress_observation(rt.input(true, true)).self_index == 2);
    let key = plant.key();
    plant.pause();
    assert_eq!(plant.key(), key, "pause keeps the same run");
    plant.resume();
    let resumed = plant.until(|rt| rt.engine.status.phase == Phase::Idle);
    assert!(!resumed.contains(&(0, true)), "the pilot already passed and is not re-tested");
    assert!(resumed.contains(&(2, false)), "the interrupted dump valve is re-tested");
    plant.step();
    assert_eq!(plant.outcome(), Some(Outcome::Passed));

    // Nitrogen test: pause while raising the second step, resume at that step.
    assert!(handle_command(&plant.state, &TelemetryCommand::NitrogenTest));
    plant.until(|rt| {
        let view = rt.engine.progress_observation(rt.input(true, true));
        view.phase == Phase::Raising && view.nitrogen_step_index == 1
    });
    plant.step();
    plant.step();
    let key = plant.key();
    let held = plant.pressure;
    plant.pause();
    assert!(plant.pressure >= held, "pause holds pressure: nothing vents");
    let controls = plant.controls();
    assert!(controls.cancel.allowed, "{:?}", controls.cancel);
    let envelope = progress::envelope(&plant.state);
    assert_eq!(envelope["controls"][0]["resume"]["allowed"], true, "{envelope}");
    assert_eq!(envelope["controls"][0]["pause"]["allowed"], false);
    assert!(envelope["controls"][0]["pause"]["reason"].as_str().unwrap().contains("paused"));
    // Fill can never start from a paused test.
    assert!(!plant.state.is_command_allowed(&TelemetryCommand::StartFill)
        || cfg!(feature = "hitl_mode"));
    plant.resume();
    assert_eq!(plant.phase(), Phase::NitrogenSetup);
    plant.until(|rt| rt.engine.status.phase == Phase::Raising);
    assert_eq!(
        plant.state.gse.lock().unwrap().engine.progress_observation(Inputs {
            now_ms: 0, pressure: None, valves: [None; 5], interlock: true, prelaunch: true
        }).nitrogen_step_index,
        1
    );
    plant.until(|rt| rt.engine.status.phase == Phase::Passed);
    assert_eq!(plant.key(), key);
    plant.step();
    assert_eq!(plant.outcome(), Some(Outcome::Passed));

    // Fill: pause mid-fill, resume through the existing Fill path.
    // The dry self-test's nitrous dwell moved no liquid.
    plant.mass = 0.0;
    plant.step();
    assert!(handle_command(&plant.state, &TelemetryCommand::StartFill));
    plant.until(|rt| rt.engine.status.phase == Phase::Filling);
    plant.until(|rt| rt.fresh_mass(rt.now()).is_some_and(|m| m.value >= 2.0));
    plant.pause();
    let resume = plant.controls().resume;
    assert!(resume.allowed, "{resume:?}");
    plant.resume();
    plant.until(|rt| rt.engine.status.phase == Phase::Filling);
    plant.until(|rt| rt.engine.status.phase == Phase::Paused && rt.fill_trim.is_none());
    plant.step();
    assert!(progress::controls(&plant.state).is_none(), "completed fill leaves no live run");
}

#[tokio::test]
async fn routed_test_cancel_from_pause_relieves_and_ends_the_run() {
    let mut plant = Plant::new().await;
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::SelfHold);
    plant.pause();
    let cancel = plant.controls().cancel;
    assert!(cancel.allowed, "{cancel:?}");
    assert!(handle_command(&plant.state, &TelemetryCommand::CancelFill));
    let received = plant.until(|rt| rt.engine.status.phase == Phase::Cancelled);
    plant.step();
    assert_eq!(plant.valves, gse_sequence::RELIEVED);
    assert!(received.iter().all(|(i, open)| !open || [1, 2].contains(i)));
    assert_eq!(plant.outcome(), Some(Outcome::Cancelled));
    assert!(progress::controls(&plant.state).is_none());
}

#[tokio::test]
async fn resume_gate_reports_server_reasons_without_changing_outputs() {
    let mut plant = Plant::new().await;
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::SelfOpen);
    let controls = plant.controls();
    assert!(controls.resume.reason.unwrap().contains("once the test is paused"));
    plant.pause();
    let before = plant.sent.lock().unwrap().len();
    // Stale valve reports: resume must wait for fresh closed confirmation.
    {
        let mut rt = plant.state.gse.lock().unwrap();
        rt.test_now_ms = Some(plant.elapsed + 5000);
    }
    let resume = plant.controls().resume;
    assert!(!resume.allowed);
    assert!(resume.reason.unwrap().contains("fresh"));
    assert!(resume_test_under_dispatch(&plant.state).is_err());
    plant.state.gse.lock().unwrap().test_now_ms = Some(plant.elapsed);
    // Lost interlock.
    let mut policy = plant.state.action_policy_snapshot();
    policy.key_enabled = false;
    plant.state.set_action_policy(policy);
    #[cfg(not(feature = "hitl_mode"))]
    {
        let resume = plant.controls().resume;
        assert!(!resume.allowed);
        assert!(resume.reason.unwrap().contains("interlock"));
    }
    plant.interlock();
    pump_test_link(&plant.state, &plant.peer);
    assert_eq!(plant.sent.lock().unwrap().len(), before, "refused resumes emit nothing");
    assert_eq!(plant.phase(), Phase::Paused);
    assert!(plant.controls().resume.allowed);
}

#[tokio::test]
async fn turbocharge_pause_ends_its_run_once_every_valve_is_confirmed_closed() {
    let (state, _sent, _peer) = button_test_state().await;
    let reserved = progress::reserve(&state).unwrap();
    let mut rt = state.gse.lock().unwrap();
    rt.test_now_ms = Some(1000);
    for key in KEYS {
        rt.observe_valve(key, false);
    }
    rt.observe_pressure(Some(0.0));
    let input = rt.input(true, true);
    rt.engine.status.phase = Phase::Turbocharge;
    rt.progress_start(Some(reserved), crate::sequence_progress::Kind::Turbocharge, input);
    // Engine-owned ACK-gated closure, as handle_command does for PauseFill.
    rt.engine.status.phase = Phase::Filling;
    let effects = rt.engine.request(Action::PauseFill, input).unwrap();
    assert!(effects.valves.iter().all(|(_, open)| !open));
    rt.progress_observe(input, crate::sequence_progress_projection::Cause::PauseAccepted);
    assert_eq!(rt.progress.current().unwrap().outcome, Outcome::Running);
    for step in 1..=10 {
        rt.test_now_ms = Some(1000 + step * 10);
        for key in KEYS {
            rt.observe_valve(key, false);
        }
        let input = rt.input(true, true);
        let _ = rt.engine.tick(input);
        rt.progress_observe(input, crate::sequence_progress_projection::Cause::Tick);
    }
    assert_eq!(rt.engine.status.phase, Phase::Paused);
    assert_eq!(rt.progress.current().unwrap().outcome, Outcome::Cancelled);
    drop(rt);
    progress::flush(&state);
    let envelope = progress::envelope(&state);
    let result = &envelope["terminal"][0];
    assert_eq!(result["outcome"], "cancelled", "{envelope}");
    assert!(result["message"].as_str().unwrap().contains("cannot resume"));
}

#[tokio::test]
async fn actions_tab_start_fill_resume_waits_for_sdk_command_retirement() {
    let mut plant = Plant::new().await;
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Idle);
    assert!(handle_command(&plant.state, &TelemetryCommand::NitrogenTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Passed);
    plant.mass = 0.0;
    plant.step();
    assert!(handle_command(&plant.state, &TelemetryCommand::StartFill));
    plant.until(|rt| rt.engine.status.phase == Phase::Filling);
    plant.until(|rt| rt.fresh_mass(rt.now()).is_some_and(|m| m.value >= 2.0));
    plant.pause();
    plant.receive();
    // An earlier valve command still owned by the SDK.
    crate::telemetry_task::queue_automatic_controller_command(
        plant.state.topology_router.get().unwrap(),
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        CLOSE[3],
    )
    .unwrap();
    assert!(!commands_retired(&plant.state));
    assert_eq!(command_allowed(&plant.state, &TelemetryCommand::StartFill), Some(false));
    let mut policy = plant.state.action_policy_snapshot();
    decorate_policy(&plant.state, &mut policy);
    let control = policy.controls.iter().find(|c| c.cmd == "StartFill").unwrap();
    assert!(!control.enabled);
    assert_eq!(control.disabled_reason.as_deref(), Some(RETIREMENT_PENDING));
    let resume = plant.controls().resume;
    assert_eq!(resume.reason.as_deref(), Some(RETIREMENT_PENDING));
    // The backend refuses the Actions-tab press itself, whatever the client shows.
    assert!(handle_command(&plant.state, &TelemetryCommand::StartFill));
    assert_eq!(plant.phase(), Phase::Paused, "no resume while a command is pending");
    assert_eq!(plant.state.gse.lock().unwrap().engine.status.message, RETIREMENT_PENDING);
    // Once delivered, StartFill resumes normally.
    plant.receive();
    assert!(commands_retired(&plant.state));
    plant.step();
    assert_eq!(command_allowed(&plant.state, &TelemetryCommand::StartFill), Some(true));
    let mut policy = plant.state.action_policy_snapshot();
    decorate_policy(&plant.state, &mut policy);
    let control = policy.controls.iter().find(|c| c.cmd == "StartFill").unwrap();
    assert!(control.enabled && control.disabled_reason.is_none(), "{control:?}");
    assert!(handle_command(&plant.state, &TelemetryCommand::StartFill));
    assert_eq!(plant.phase(), Phase::FillSetup);
    plant.until(|rt| rt.engine.status.phase == Phase::Filling);
}

#[tokio::test]
async fn routed_nitrogen_leak_holds_pressure_until_dump_then_ends_at_zero() {
    let mut plant = Plant::new().await;
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Idle);
    plant.step();
    assert!(handle_command(&plant.state, &TelemetryCommand::NitrogenTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Holding);
    plant.receive();
    plant.leak = 1.0;
    let commanded = plant.until(|rt| rt.engine.status.phase == Phase::Paused);
    plant.leak = 0.0;
    assert!(
        !commanded.iter().any(|&(i, open)| open && (i == 1 || i == 2)),
        "a leak verdict never vents or dumps: {commanded:?}"
    );
    assert_eq!(plant.valves, gse_sequence::CLOSED);
    let held = plant.pressure;
    assert!(held > 10.0, "pressure is held for the leak search");
    {
        let rt = plant.state.gse.lock().unwrap();
        assert!(rt.engine.status.message.starts_with("Nitrogen test failed: "));
        assert!(rt.engine.status.message.ends_with("Holding pressure for leak search. Dump when ready."));
        #[cfg(not(feature = "hitl_mode"))]
        assert!(!rt.operational.saved.qualification.nitrogen_satisfied());
    }
    for _ in 0..200 {
        plant.step();
    }
    assert_eq!(plant.phase(), Phase::Paused, "waits indefinitely");
    assert_eq!(plant.pressure, held);
    let controls = plant.controls();
    let choice = controls.hold_choice.clone().expect("Dump / Cold flow prompt");
    assert!(choice.dump.allowed, "Dump is available: {:?}", choice.dump);
    assert!(controls.cancel.allowed, "Cancel is available");
    assert!(!controls.resume.allowed, "a leak verdict cannot resume");
    {
        let _dispatch = DISPATCH_GATE.lock().unwrap();
        dump_test_hold_under_dispatch(&plant.state).unwrap();
    }
    // The same run stays open (its modal stays) until the tank is empty.
    plant.step();
    assert_eq!(plant.outcome(), Some(Outcome::Running), "Dump keeps the run open");
    let dumped = plant.until(|rt| rt.engine.status.phase == Phase::Cancelled);
    assert!(dumped.contains(&(1, true)) && dumped.contains(&(2, true)));
    assert!(!dumped.contains(&(0, true)), "Dump never opens the main valve");
    assert_eq!(plant.pressure, 0.0);
    plant.step();
    assert_eq!(plant.outcome(), Some(Outcome::Failed));
}

#[tokio::test]
async fn routed_cancel_at_the_nitrogen_hold_relieves_and_ends_the_run_at_once() {
    let mut plant = Plant::new().await;
    assert!(handle_command(&plant.state, &TelemetryCommand::ValveSelfTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Idle);
    plant.step();
    assert!(handle_command(&plant.state, &TelemetryCommand::NitrogenTest));
    plant.until(|rt| rt.engine.status.phase == Phase::Holding);
    plant.receive();
    plant.leak = 1.0;
    plant.until(|rt| rt.engine.status.phase == Phase::Paused);
    plant.leak = 0.0;
    let held = plant.pressure;
    assert!(handle_command(&plant.state, &TelemetryCommand::CancelFill));
    let relieved = plant.until(|rt| rt.engine.status.phase == Phase::Cancelled);
    assert!(relieved.contains(&(1, true)) && relieved.contains(&(2, true)));
    assert!(!relieved.contains(&(0, true)), "Cancel never opens the main valve");
    plant.step();
    assert!(plant.outcome().is_some_and(|o| o.terminal()), "the run ends: the modal can close");
    assert!(progress::controls(&plant.state).is_none());
    assert!(plant.pressure < held, "dump and vent stay open after the run ends");
    assert_eq!(plant.valves, gse_sequence::RELIEVED);
}
