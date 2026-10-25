use super::*;
use crate::types::Board;
use gse_sequence::qualification::{Fault, Milestone};

pub(super) fn operational_action_allowed(rt: &Runtime, action: Action) -> bool {
    if matches!(action, Action::PauseFill | Action::CancelFill) {
        return true;
    }
    let milestone = match action {
        Action::SelfTest => Milestone::SelfTest,
        Action::NitrogenTest => Milestone::NitrogenTest,
        Action::StartFill => Milestone::FillTarget,
        _ => unreachable!(),
    };
    crate::checklists::paired::qualification_allowed(
        rt.operational.saved.checklist_digest.as_deref(),
        rt.operational.has_checklist_evidence(),
    ) && rt.operational.saved.pending_checklists.is_empty()
        && rt.operational.topology_stable
        && rt.operational.startup_configured
        && rt.operational.persist_error.is_none()
        && rt.operational.saved.qualification.permits(milestone)
}
fn flight_routes(state: &AppState) -> bool {
    let recent = state.operational_board_recent(Board::FlightComputer, 3_000);
    recent
        && state.topology_router.get().is_some_and(|r| {
            r.export_topology().routes.iter().any(|s| {
                s.reachable_endpoints
                    .contains(&crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"))
            })
        })
}
pub(super) fn operational_launch_allowed(state: &AppState, rt: &Runtime) -> bool {
    let now = rt.now();
    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    crate::checklists::paired::qualification_allowed(
        rt.operational.saved.checklist_digest.as_deref(),
        rt.operational.has_checklist_evidence(),
    ) && rt.operational.saved.pending_checklists.is_empty()
        && rt.operational.saved.qualification.launch_qualified()
        && rt.operational.persist_error.is_none()
        && rt.operational.topology_stable
        && rt.operational.saved.fill_target_kg == Some(target)
        && rt
            .fresh_mass(now)
            .is_some_and(|mass| rt.fill_cutoff.target_met(mass.value, target))
        && matches!(
            rt.engine.status.phase,
            Phase::Passed | Phase::Paused | Phase::Idle
        )
        && rt.pressure.is_some_and(|p| {
            p.value.is_finite()
                && p.at_ms <= now
                && now - p.at_ms <= 2_000
                && rt
                    .engine
                    .config
                    .pressure_limit(rt.engine.nitrous_target_psi, 50.0)
                    .is_ok_and(|limit| p.value < limit)
        })
        && rt
            .valves
            .iter()
            .all(|v| v.is_some_and(|v| !v.open && v.at_ms <= now && now - v.at_ms <= 2_000))
        && (cfg!(feature = "test_fire_mode")
            || (flight_routes(state) && crate::antenna::rocket_launch_fix_locked()))
}
fn topology(state: &AppState) -> (bool, String) {
    let required = [
        Board::GatewayBoard,
        Board::ValveBoard,
        Board::ActuatorBoard,
        Board::DaqBoard,
    ];
    let fresh = required
        .iter()
        .all(|board| state.operational_board_recent(*board, 3_000));
    let Some(router) = state.topology_router.get() else {
        return (false, String::new());
    };
    let snapshot = router.export_topology();
    let endpoints = ["VALVE_BOARD", "ACTUATOR_BOARD"];
    let routes = endpoints.iter().all(|name| {
        snapshot.routes.iter().any(|side| {
            side.reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint(name))
        })
    });
    let mut keys: Vec<_> = snapshot
        .routes
        .iter()
        .filter(|side| {
            endpoints.iter().any(|name| {
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
    (fresh && routes, keys.join("|"))
}
fn fault(rt: &mut Runtime, reason: Fault, message: &str) -> Effects {
    rt.operational.saved.qualification.latch(reason);
    rt.operational.running = false;
    rt.operational.qd = QdWait::default();
    let _ = rt.operational.persist();
    rt.engine.fault(rt.now(), message)
}
pub(super) fn complete_operational_milestones(rt: &mut Runtime, previous: Phase) {
    let phase = rt.engine.status.phase;
    if previous == phase || !rt.operational.running {
        return;
    }
    let milestone = if previous == Phase::SelfFinish && phase == Phase::Idle {
        rt.operational.saved.baseline = rt.engine.status.baseline.clone();
        Some(Milestone::SelfTest)
    } else if phase == Phase::Passed {
        rt.operational.saved.baseline = rt.engine.status.baseline.clone();
        Some(Milestone::NitrogenTest)
    } else if previous == Phase::PauseSetup
        && phase == Phase::Paused
        && rt.operational.target_reached.is_some_and(|target| {
            rt.fresh_mass(rt.now())
                .is_some_and(|mass| rt.fill_cutoff.target_met(mass.value, target))
        })
    {
        rt.operational.saved.fill_target_kg = rt.operational.target_reached;
        Some(Milestone::FillTarget)
    } else {
        None
    };
    if matches!(phase, Phase::Cancelled | Phase::Paused) {
        rt.operational.running = false;
        if phase == Phase::Cancelled && rt.operational.saved.qualification.nitrogen_satisfied() {
            rt.engine.status.nitrogen_passed = rt
                .operational
                .saved
                .qualification
                .completed(Milestone::NitrogenTest);
            rt.engine.status.nitrogen_overridden = rt
                .operational
                .saved
                .qualification
                .nitrogen_override()
                .is_some();
            rt.engine.status.baseline = rt.operational.saved.baseline.clone();
            rt.engine.status.phase = Phase::Passed;
        }
    }
    if let Some(milestone) = milestone {
        if let Err(error) = rt.operational.complete(milestone) {
            rt.engine.status.message =
                format!("Qualification could not be saved: {error}. Manual reset required.");
        } else if let Err(error) = rt.operational.flush_checklists() {
            rt.engine.status.message =
                format!("Test passed, but checklist progress could not be saved: {error}");
        }
        rt.operational.running = false;
    }
    if phase == Phase::Fault {
        rt.operational
            .saved
            .qualification
            .latch(Fault::MissingTelemetry);
        let _ = rt.operational.persist();
        rt.operational.running = false;
        rt.operational.qd = QdWait::default();
    }
}
/// Returns true when automation must not advance this tick.
pub(super) fn operational_tick(state: &Arc<AppState>, interlock: bool) -> bool {
    if state
        .gse
        .lock()
        .unwrap()
        .operational
        .saved
        .qualification
        .stage()
        == gse_sequence::qualification::MissionStage::Fire
    {
        return true;
    }
    let stage = *state.state.lock().unwrap();
    // No GroundStation-generated abort or valve correction from Launch onward.
    if !matches!(
        stage,
        FlightState::Startup
            | FlightState::Idle
            | FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
            | FlightState::Armed
    ) {
        return true;
    }
    let (healthy, key) = topology(state);
    let mut effects = Effects::default();
    let mut launch = false;
    let mut rt = state.gse.lock().unwrap();
    let now = rt.now();
    rt.operational.topology(now, healthy, key);
    if !rt.operational.running {
        rt.engine.nitrous_target_psi = state.fill_targets_snapshot().nitrous.target_pressure_psi;
    }
    if !rt.operational.startup_configured
        && rt.operational.topology_stable
        && interlock
        && rt.operational.saved.qualification.fault().is_none()
        && rt.operational.persist_error.is_none()
    {
        let hold_filled = rt
            .operational
            .saved
            .qualification
            .completed(Milestone::FillTarget);
        let pressure_ok = rt.pressure.is_some_and(|p| {
            p.value.is_finite()
                && p.at_ms <= now
                && now - p.at_ms <= 2_000
                && (hold_filled
                    || rt
                        .engine
                        .config
                        .maximum_zero_offset_psi
                        .is_some_and(|zero| p.value.abs() <= zero))
        });
        if pressure_ok {
            effects = rt.engine.prepare_startup(now, hold_filled);
            rt.operational.startup_configured = true;
            rt.operational.running = true;
            rt.operational.run_generation = rt.operational.saved.qualification.generation();
        }
    }
    if rt.operational.saved.qualification.fault().is_none() {
        let operating = rt.operational.running
            || rt.operational.saved.qualification.stage()
                != gse_sequence::qualification::MissionStage::Idle
            || !matches!(stage, FlightState::Startup | FlightState::Idle);
        if operating && !healthy {
            effects = fault(
                &mut rt,
                Fault::TopologyLost,
                "Fill-system connectivity lost",
            );
        } else if rt.operational.running && !interlock {
            effects = fault(&mut rt, Fault::ValveMismatch, "Interlock lost");
        }
    }
    if rt.operational.qd.queued() && matches!(rt.operational.qd.status(now), QdStatus::AckTimedOut)
    {
        rt.operational.qd = QdWait::default();
        state.add_temporary_notification("QD acknowledgement timed out; queued launch cancelled");
    }
    if rt.operational.qd.queued() && matches!(rt.operational.qd.status(now), QdStatus::Ready) {
        // Timer is never authority to launch: every prerequisite is rechecked.
        if interlock && operational_launch_allowed(state, &rt) {
            launch = rt.operational.qd.take_ready_launch(now);
        } else {
            rt.operational.qd = QdWait::default();
            state.add_temporary_notification(
                "Queued launch cancelled: live launch prerequisites changed",
            );
        }
    }
    let skip = !rt.operational.running || !effects.valves.is_empty();
    drop(rt);
    apply(state, effects);
    if launch && state.cmd_tx.try_send(TelemetryCommand::Launch).is_err() {
        state.add_notification("Queued launch was not dispatched: command queue unavailable");
    }
    skip
}
pub(super) fn handle_operational_command(state: &Arc<AppState>, cmd: &TelemetryCommand) -> bool {
    let name = crate::sequences::command_name(cmd);
    if name == "RetractPlumbing" && !cfg!(feature = "test_fire_mode") {
        let Some(router) = state.topology_router.get() else {
            return true;
        };
        let mut rt = state.gse.lock().unwrap();
        if !operational_launch_allowed(state, &rt) {
            return true;
        }
        if matches!(
            rt.operational.qd.status(rt.now()),
            QdStatus::AwaitingAck | QdStatus::Settling { .. } | QdStatus::Ready
        ) {
            return true;
        }
        match router.log_queue(
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8],
        ) {
            Ok(()) => {
                let now = rt.now();
                rt.operational.qd.dispatched(now);
                rt.operational.qd_dispatch_utc_ms =
                    Some(crate::telemetry_task::get_current_timestamp_ms());
                drop(rt);
                state.set_pending_umbilical_valve_state(
                    crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8,
                    true,
                );
                crate::telemetry_task::flush_command_tx(router, "QD retract");
            }
            Err(error) => {
                drop(rt);
                state.add_notification(format!("QD was not dispatched: {error}"));
            }
        }
        return true;
    }
    if matches!(name, "Launch" | "GroundStationLaunch") && !cfg!(feature = "test_fire_mode") {
        let mut rt = state.gse.lock().unwrap();
        let now = rt.now();
        if !matches!(rt.operational.qd.status(now), QdStatus::Ready) {
            match rt.operational.qd.queue_launch(now) {
                Ok(()) => {
                    let message=match rt.operational.qd.status(now) {
                        QdStatus::Settling{remaining_ms}=>format!("Launch queued: QD settling, {:.1} seconds remaining",remaining_ms as f64/1000.0),
                        _=>"Launch queued: waiting for QD acknowledgement, then five seconds settling".into(),
                    };
                    drop(rt);
                    state.add_temporary_notification(message);
                }
                Err(error) => {
                    drop(rt);
                    state.add_temporary_notification(error);
                }
            }
            return true;
        }
    }
    false
}

pub(super) fn check_report_safety(state: &Arc<AppState>) {
    if state
        .gse
        .lock()
        .unwrap()
        .operational
        .saved
        .qualification
        .stage()
        == gse_sequence::qualification::MissionStage::Fire
    {
        return;
    }
    let stage = *state.state.lock().unwrap();
    if !matches!(
        stage,
        FlightState::Startup
            | FlightState::Idle
            | FlightState::PreFill
            | FlightState::FillTest
            | FlightState::NitrogenFill
            | FlightState::NitrousFill
            | FlightState::Armed
    ) {
        return;
    }
    let mut rt = state.gse.lock().unwrap();
    if rt.operational.saved.qualification.fault().is_some() {
        return;
    }
    let phase = rt.engine.status.phase;
    let nitrous_session = matches!(
        phase,
        Phase::FillSetup | Phase::Filling | Phase::PauseSetup | Phase::Paused
    );
    let unexpected_supply = if nitrous_session && rt.valves[3].is_some_and(|v| v.open) {
        Some("Nitrogen opened during nitrous fill")
    } else if matches!(
        phase,
        Phase::Raising | Phase::Settling | Phase::Holding | Phase::Paused
    ) && rt.valves[4].is_some_and(|v| v.open)
    {
        Some("Nitrous opened outside nitrous fill")
    } else {
        None
    };
    if let Some(message) = unexpected_supply {
        let effects = fault(&mut rt, Fault::UnsafeSupplyFlow, message);
        drop(rt);
        apply(state, effects);
        return;
    }
    // Correct resting pilot/dump/vent states only after a setup phase has
    // completed. Transitional reports still receive the engine's ACK window.
    // Never automatically reopen a supply from an unsolicited report.
    let desired = match phase {
        Phase::Filling => Some([false, true, false]),
        Phase::Raising | Phase::Settling | Phase::Holding | Phase::Paused => Some([false; 3]),
        _ => None,
    };
    let now = rt.now();
    if let Some(desired) = desired {
        if rt
            .operational
            .last_correction_ms
            .is_some_and(|last| now.saturating_sub(last) < 500)
        {
            return;
        }
        let valves: Vec<_> = desired
            .into_iter()
            .enumerate()
            .filter(|(index, expected)| {
                rt.valves[*index].is_some_and(|value| {
                    value.at_ms <= now && now - value.at_ms <= 2000 && value.open != *expected
                })
            })
            .collect();
        if !valves.is_empty() {
            rt.operational.last_correction_ms = Some(now);
            drop(rt);
            apply(
                state,
                Effects {
                    valves,
                    notification: Some("Restoring sequence valve configuration".into()),
                    ..Effects::default()
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> Arc<AppState> {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        let router = Arc::new(sedsnet::router::Router::new(
            sedsnet::router::RouterConfig::new([]),
        ));
        let side = router.add_side_packed("simulated_fill", |_| Ok(()));
        let announce = sedsnet::discovery::build_discovery_announce(
            "GB",
            123,
            &[
                crate::telemetry_schema::endpoint("VALVE_BOARD"),
                crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
            ],
        )
        .unwrap();
        router
            .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), side)
            .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        state.topology_router.set(router).unwrap();
        for board in [
            Board::GatewayBoard,
            Board::ValveBoard,
            Board::ActuatorBoard,
            Board::DaqBoard,
        ] {
            state.mark_board_seen(
                board.sender_id(),
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
        let (_, key) = topology(&state);
        let mut rt = state.gse.lock().unwrap();
        rt.clock = Instant::now() - std::time::Duration::from_secs(3);
        rt.operational.topology(0, true, key.clone());
        rt.operational.topology(3000, true, key);
        rt.operational.startup_configured = true;
        rt.operational.path = Some(std::env::temp_dir().join(format!(
            "gse-runtime-{}-{}.json",
            std::process::id(),
            rand::random::<u128>()
        )));
        rt.engine.config.maximum_zero_offset_psi = Some(5.0);
        rt.engine.config.dry_self_test_confirmed = true;
        rt.observe_pressure(Some(0.0));
        rt.observe_mass(Some(0.0));
        for (key, open) in KEYS.into_iter().zip(gse_sequence::CLOSED) {
            rt.observe_valve(key, open);
        }
        drop(rt);
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        state
    }
    fn cleanup(state: &AppState) {
        if let Some(path) = &state.gse.lock().unwrap().operational.path {
            let _ = std::fs::remove_file(path);
        }
    }
    #[tokio::test]
    async fn operator_procedure_completes_over_routed_commands_and_persists_each_check() {
        use crate::gse::tests::{button_test_state, confirm_test_valves, pump_test_link};
        let (state, sent, peer) = button_test_state().await;
        let path =
            std::env::temp_dir().join(format!("gse-procedure-{}.json", rand::random::<u128>()));
        for board in [
            Board::GatewayBoard,
            Board::ValveBoard,
            Board::ActuatorBoard,
            Board::DaqBoard,
        ] {
            state.mark_board_seen(
                board.sender_id(),
                crate::telemetry_task::get_current_timestamp_ms(),
            );
        }
        let (_, key) = topology(&state);
        {
            let mut rt = state.gse.lock().unwrap();
            rt.clock = Instant::now() - std::time::Duration::from_secs(3);
            rt.operational.path = Some(path.clone());
            rt.operational.topology(0, true, key.clone());
            rt.operational.topology(3000, true, key);
            rt.operational.startup_configured = true;
            rt.engine.config.dry_self_test_confirmed = true;
        }
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        let mut valves = gse_sequence::RELIEVED;
        confirm_test_valves(&state, valves);
        let mut elapsed = 3000;
        let mut consumed = 0;
        // The fake routed peer does not emit board heartbeats. Model them
        // explicitly for this connected-procedure scenario, including slow Pi
        // test runners; production freshness deadlines remain unchanged.
        let refresh_boards = || {
            for board in [
                Board::GatewayBoard,
                Board::ValveBoard,
                Board::ActuatorBoard,
                Board::DaqBoard,
            ] {
                state.mark_board_seen(
                    board.sender_id(),
                    crate::telemetry_task::get_current_timestamp_ms(),
                );
            }
        };
        for (command, milestone, done) in [
            ("ValveSelfTest", Milestone::SelfTest, Phase::Idle),
            ("NitrogenTest", Milestone::NitrogenTest, Phase::Passed),
            ("StartFill", Milestone::FillTarget, Phase::Paused),
        ] {
            let cmd: TelemetryCommand = serde_json::from_value(serde_json::json!(command)).unwrap();
            refresh_boards();
            // The routed fixture has no physical panel. A policy refresh on
            // Raspberry Pi correctly fails closed for its absent enable input;
            // supply the operator's simulated enable before each request.
            let mut policy = state.action_policy_snapshot();
            policy.key_enabled = true;
            policy.software_buttons_enabled = true;
            state.set_action_policy(policy);
            assert_eq!(command_allowed(&state, &cmd), Some(true), "{command}");
            assert!(handle_command(&state, &cmd));
            for _ in 0..1200 {
                pump_test_link(&state, &peer);
                refresh_boards();
                {
                    let commands = sent.lock().unwrap();
                    for (kind, bytes) in &commands[consumed..] {
                        assert_eq!(bytes.len(), 1);
                        let candidates = if kind == "VALVE_COMMAND" { 0..3 } else { 3..5 };
                        let index = candidates
                            .into_iter()
                            .find(|&i| bytes[0] == OPEN[i] || bytes[0] == CLOSE[i])
                            .expect("only GSE valves are actuated");
                        valves[index] = bytes[0] == OPEN[index];
                    }
                    consumed = commands.len();
                }
                elapsed += 100;
                {
                    let mut rt = state.gse.lock().unwrap();
                    rt.clock = Instant::now() - std::time::Duration::from_millis(elapsed);
                }
                confirm_test_valves(&state, valves);
                {
                    let mut rt = state.gse.lock().unwrap();
                    let phase = rt.engine.status.phase;
                    let pressure =
                        if matches!(phase, Phase::Raising | Phase::Settling | Phase::Holding) {
                            rt.engine.status.current_step_psi
                        } else {
                            0.0
                        };
                    rt.observe_pressure(Some(pressure));
                    let mass = if phase == Phase::Filling
                        || phase == Phase::PauseSetup
                        || phase == Phase::Paused
                    {
                        state.fill_targets_snapshot().nitrous.target_mass_kg
                    } else {
                        0.0
                    };
                    rt.observe_mass(Some(mass));
                    if phase == Phase::Raising {
                        assert_eq!(valves, [false, false, false, true, false]);
                    }
                    if phase == Phase::Holding {
                        assert_eq!(valves, gse_sequence::CLOSED);
                    }
                }
                tick(&state, true);
                let rt = state.gse.lock().unwrap();
                assert_ne!(
                    rt.engine.status.phase,
                    Phase::Fault,
                    "{command}: {}",
                    rt.engine.status.message
                );
                if !rt.operational.running && rt.engine.status.phase == done {
                    break;
                }
            }
            let saved = operational::Saved::load(&path).unwrap();
            assert!(
                saved.qualification.completed(milestone),
                "{command} must persist completion"
            );
        }
        assert_eq!(valves, gse_sequence::CLOSED);
        assert!(
            operational::Saved::load(&path)
                .unwrap()
                .qualification
                .launch_qualified()
        );
        assert!(
            consumed > 10,
            "the test must exercise routed valve commands"
        );
        cleanup(&state);
    }
    #[tokio::test]
    async fn runtime_requires_self_test_before_nitrogen_and_saves_completion() {
        let state = fixture().await;
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::NitrogenTest),
            Some(false)
        );
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.running = true;
            rt.engine.status.phase = Phase::Idle;
            complete_operational_milestones(&mut rt, Phase::SelfFinish);
            let saved = operational::Saved::load(rt.operational.path.as_ref().unwrap()).unwrap();
            assert!(saved.qualification.completed(Milestone::SelfTest));
        }
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::NitrogenTest),
            Some(true)
        );
        handle_command(&state, &TelemetryCommand::NitrogenTest);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::BaselineSetup
        );
        cleanup(&state);
    }
    #[tokio::test]
    async fn incompatible_nitrogen_report_faults_immediately_but_never_after_launch() {
        let state = fixture().await;
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.running = true;
            rt.engine.status.phase = Phase::Filling;
            rt.observe_valve(9, true);
        }
        check_report_safety(&state);
        {
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Fault);
            assert_eq!(
                rt.operational.saved.qualification.fault(),
                Some(Fault::UnsafeSupplyFlow)
            );
        }
        assert_eq!(state.get_pending_umbilical_valve_state(9), Some(false));
        state.set_local_flight_state(FlightState::Launch);
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational.saved = Default::default();
            rt.engine.status.phase = Phase::Filling;
            rt.operational.running = true;
        }
        check_report_safety(&state);
        assert!(operational_tick(&state, false));
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Filling
        );
        assert_eq!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .qualification
                .fault(),
            None
        );
        cleanup(&state);
    }
    #[tokio::test]
    async fn resting_valve_mismatch_is_corrected_without_reopening_supplies_or_after_launch() {
        let state = fixture().await;
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.status.phase = Phase::Paused;
            rt.observe_valve(1, true); // Vent must stay closed on a filled hold.
        }
        check_report_safety(&state);
        assert_eq!(state.get_pending_umbilical_valve_state(1), Some(false));
        assert_eq!(state.get_pending_umbilical_valve_state(9), None);
        assert_eq!(state.get_pending_umbilical_valve_state(10), None);
        state.set_local_flight_state(FlightState::Launch);
        state.set_pending_umbilical_valve_state(1, true);
        check_report_safety(&state);
        assert_eq!(state.get_pending_umbilical_valve_state(1), Some(true));
        cleanup(&state);
    }
    #[tokio::test]
    async fn nitrous_opening_during_nitrogen_test_latches_immediate_crossflow_fault() {
        let state = fixture().await;
        {
            let mut rt = state.gse.lock().unwrap();
            rt.engine.status.phase = Phase::Raising;
            rt.operational.running = true;
            rt.observe_valve(9, true);
            rt.observe_valve(10, true);
        }
        check_report_safety(&state);
        assert_eq!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .qualification
                .fault(),
            Some(Fault::UnsafeSupplyFlow)
        );
        assert_eq!(state.get_pending_umbilical_valve_state(9), Some(false));
        assert_eq!(state.get_pending_umbilical_valve_state(10), Some(false));
        cleanup(&state);
    }
    #[tokio::test]
    async fn stale_cached_qd_status_and_wrong_sender_never_start_settling() {
        let state = fixture().await;
        let key = crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8;
        {
            let mut rt = state.gse.lock().unwrap();
            let now = rt.now();
            rt.operational.qd.dispatched(now);
            rt.operational.qd_dispatch_utc_ms = Some(1000);
        }
        for (sender, on, time) in [("AB", true, 999), ("VB", true, 1001), ("AB", false, 1001)] {
            super::super::qd_confirmation(&state, sender, key, on, time);
        }
        assert_eq!(
            state.gse.lock().unwrap().operational.qd.status(3000),
            QdStatus::AwaitingAck
        );
        super::super::qd_confirmation(&state, "AB", key, true, 1001);
        assert!(matches!(
            state.gse.lock().unwrap().operational.qd.status(3000),
            QdStatus::Settling { remaining_ms: 5000 }
        ));
        cleanup(&state);
    }
    #[cfg(not(feature = "test_fire_mode"))]
    #[tokio::test]
    async fn flight_launch_waits_five_seconds_from_qd_ack_and_rechecks_live_gates() {
        let mut state = fixture().await;
        let (commands, mut received) = tokio::sync::mpsc::channel(8);
        Arc::get_mut(&mut state).unwrap().cmd_tx = commands;
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        {
            let mut rt = state.gse.lock().unwrap();
            for milestone in [
                Milestone::SelfTest,
                Milestone::NitrogenTest,
                Milestone::FillTarget,
            ] {
                rt.operational
                    .saved
                    .qualification
                    .complete(milestone, 0)
                    .unwrap();
            }
            rt.operational.saved.fill_target_kg = Some(target);
            rt.engine.status.phase = Phase::Paused;
            rt.observe_mass(Some(target));
            assert!(
                !operational_launch_allowed(&state, &rt),
                "flight requires FC routes and GPS, even with completed fill checks"
            );
        }
        let router = state.topology_router.get().unwrap();
        let radio = router.add_side_packed("simulated_radio", |_| Ok(()));
        let announce = sedsnet::discovery::build_discovery_announce(
            "FC",
            123,
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
        )
        .unwrap();
        router
            .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&announce), radio)
            .unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        state.mark_board_seen("FC", crate::telemetry_task::get_current_timestamp_ms());
        crate::antenna::observe(&crate::types::TelemetryRow {
            timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
            data_type: "GPS_DATA".into(),
            sender_id: "RF".into(),
            values: vec![Some(43.0), Some(-78.0), Some(150.0)],
        });
        assert!(operational_launch_allowed(
            &state,
            &state.gse.lock().unwrap()
        ));
        assert!(handle_operational_command(
            &state,
            &TelemetryCommand::RetractPlumbing
        ));
        assert!(handle_operational_command(
            &state,
            &TelemetryCommand::Launch
        ));
        assert!(
            received.try_recv().is_err(),
            "queued launch cannot run before a fresh QD ACK"
        );
        let dispatched = state
            .gse
            .lock()
            .unwrap()
            .operational
            .qd_dispatch_utc_ms
            .unwrap();
        super::super::qd_confirmation(
            &state,
            "AB",
            crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8,
            true,
            dispatched + 1,
        );
        let acknowledged_at = state.gse.lock().unwrap().now();
        for (advance, should_launch) in [(4900, false), (5100, true)] {
            let mut rt = state.gse.lock().unwrap();
            rt.clock = Instant::now() - std::time::Duration::from_millis(acknowledged_at + advance);
            rt.observe_pressure(Some(0.0));
            rt.observe_mass(Some(target));
            for key in KEYS {
                rt.observe_valve(key, false);
            }
            drop(rt);
            operational_tick(&state, true);
            if should_launch {
                assert!(matches!(received.try_recv(), Ok(TelemetryCommand::Launch)));
            } else {
                assert!(received.try_recv().is_err());
            }
        }
        operational_tick(&state, true);
        assert!(
            received.try_recv().is_err(),
            "the queued launch is dispatched once"
        );
        cleanup(&state);
    }
    #[tokio::test]
    async fn stabilized_below_target_does_not_qualify_launch() {
        let state = fixture().await;
        let mut rt = state.gse.lock().unwrap();
        rt.operational
            .saved
            .qualification
            .complete(Milestone::SelfTest, 0)
            .unwrap();
        rt.operational.saved.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.0,
            min_psi: 0.0,
            max_psi: 0.0,
            noise_psi: 0.1,
            samples: 20,
        });
        rt.operational
            .saved
            .qualification
            .complete(Milestone::NitrogenTest, 0)
            .unwrap();
        rt.operational.running = true;
        rt.engine.status.phase = Phase::Paused;
        complete_operational_milestones(&mut rt, Phase::PauseSetup);
        assert!(!rt.operational.saved.qualification.launch_qualified());
        drop(rt);
        cleanup(&state);
    }
    #[tokio::test]
    async fn lost_fill_board_latches_fault_and_cancels_pending_launch() {
        let state = fixture().await;
        state.gse.lock().unwrap().operational.running = true;
        state
            .board_status
            .lock()
            .unwrap()
            .get_mut(&Board::ValveBoard)
            .unwrap()
            .last_seen_instant = Some(Instant::now() - std::time::Duration::from_secs(4));
        assert!(operational_tick(&state, true));
        assert_eq!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .qualification
                .fault(),
            Some(Fault::TopologyLost)
        );
        cleanup(&state);
    }
    #[tokio::test]
    async fn cold_flow_override_requires_real_self_test_and_live_safe_inputs() {
        let state = fixture().await;
        assert!(apply_cold_flow_override(&state, "alice", "External cold flow reviewed").is_err());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.operational
                .saved
                .qualification
                .complete(Milestone::SelfTest, 0)
                .unwrap();
            rt.operational.saved.baseline = Some(gse_sequence::Baseline {
                average_psi: 0.0,
                min_psi: 0.0,
                max_psi: 0.0,
                noise_psi: 0.1,
                samples: 50,
            });
            rt.observe_valve(9, true);
        }
        assert!(apply_cold_flow_override(&state, "alice", "External cold flow reviewed").is_err());
        {
            let mut rt = state.gse.lock().unwrap();
            rt.observe_valve(9, false);
            rt.observe_pressure(Some(2.0));
        }
        assert!(apply_cold_flow_override(&state, "alice", "External cold flow reviewed").is_err());
        state.gse.lock().unwrap().observe_pressure(Some(0.0));
        apply_cold_flow_override(&state, "alice", "External cold flow reviewed").unwrap();
        let rt = state.gse.lock().unwrap();
        assert!(!rt.engine.status.nitrogen_passed);
        assert!(rt.engine.status.nitrogen_overridden);
        assert!(rt.engine.allows(Action::StartFill, rt.input(true, true)));
        let saved = operational::Saved::load(rt.operational.path.as_ref().unwrap()).unwrap();
        assert_eq!(
            saved.qualification.nitrogen_override().unwrap().operator,
            "alice"
        );
        assert!(!saved.qualification.completed(Milestone::NitrogenTest));
        drop(rt);
        cleanup(&state);
    }
    #[tokio::test]
    async fn durable_fire_intent_is_once_only_and_prevents_restart_abort_or_valve_setup() {
        let state = fixture().await;
        let path = armed_fixture_for_test(&state, state.topology_router.get().unwrap());
        commit_fire_intent(&state).unwrap();
        assert!(commit_fire_intent(&state).is_err());
        let saved = operational::Saved::load(&path).unwrap();
        assert_eq!(
            saved.qualification.stage(),
            gse_sequence::qualification::MissionStage::Fire
        );
        assert!(!saved.qualification.nitrogen_satisfied());
        assert!(saved.pending_checklists.is_empty());
        // Missing FC flight state immediately after restart must not reactivate GSE automation.
        state.set_local_flight_state(FlightState::Idle);
        assert!(!automatic_abort_allowed(&state));
        assert!(operational_tick(&state, false));
        check_report_safety(&state);
        assert_eq!(
            state
                .gse
                .lock()
                .unwrap()
                .operational
                .saved
                .qualification
                .fault(),
            None
        );
        assert!(state.gse.lock().unwrap().engine.claimed);
        cleanup(&state);
        let _ = std::fs::remove_file(path);
    }
    #[tokio::test]
    async fn paused_fill_and_armed_mission_keep_connectivity_protection_while_fc_state_is_idle() {
        for armed in [false, true] {
            let state = fixture().await;
            {
                let mut rt = state.gse.lock().unwrap();
                for milestone in [Milestone::SelfTest, Milestone::NitrogenTest] {
                    rt.operational
                        .saved
                        .qualification
                        .complete(milestone, 0)
                        .unwrap();
                }
                rt.operational.saved.baseline = Some(gse_sequence::Baseline {
                    average_psi: 0.0,
                    min_psi: 0.0,
                    max_psi: 0.0,
                    noise_psi: 0.1,
                    samples: 50,
                });
                rt.operational.saved.qualification.begin_fill(0).unwrap();
                if armed {
                    rt.operational
                        .saved
                        .qualification
                        .complete(Milestone::FillTarget, 0)
                        .unwrap();
                    rt.operational.saved.fill_target_kg = Some(8.0);
                }
                rt.operational.running = false;
                rt.engine.status.phase = Phase::Paused;
            }
            state
                .board_status
                .lock()
                .unwrap()
                .get_mut(&Board::ValveBoard)
                .unwrap()
                .last_seen_instant = Some(Instant::now() - std::time::Duration::from_secs(4));
            assert!(operational_tick(&state, true));
            assert_eq!(
                state
                    .gse
                    .lock()
                    .unwrap()
                    .operational
                    .saved
                    .qualification
                    .fault(),
                Some(Fault::TopologyLost)
            );
            cleanup(&state);
        }
    }
    #[tokio::test]
    async fn accepted_mass_cutoff_can_arm_without_requiring_an_unreachable_exact_float_target() {
        let state = fixture().await;
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        let mut rt = state.gse.lock().unwrap();
        for milestone in [Milestone::SelfTest, Milestone::NitrogenTest] {
            rt.operational
                .saved
                .qualification
                .complete(milestone, 0)
                .unwrap();
        }
        rt.operational.saved.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.0,
            min_psi: 0.0,
            max_psi: 0.0,
            noise_psi: 0.1,
            samples: 50,
        });
        rt.operational.saved.qualification.begin_fill(0).unwrap();
        rt.operational.running = true;
        rt.operational.target_reached = Some(target);
        rt.engine.status.phase = Phase::Paused;
        rt.observe_mass(Some(target * 0.996));
        complete_operational_milestones(&mut rt, Phase::PauseSetup);
        assert_eq!(
            rt.operational.saved.qualification.stage(),
            gse_sequence::qualification::MissionStage::Armed
        );
        assert!(rt.operational.saved.qualification.launch_qualified());
        assert!(rt.fill_cutoff.target_met(target * 0.996, target));
        assert!(!rt.fill_cutoff.target_met(target * 0.98, target));
        assert!(!rt.fill_cutoff.target_met(f32::NAN, target));
        drop(rt);
        cleanup(&state);
    }
}
