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
    !rt.operational.saved.automatic_abort_claimed && crate::checklists::paired::qualification_allowed(
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
    !rt.operational.saved.automatic_abort_claimed && crate::checklists::paired::qualification_allowed(
        rt.operational.saved.checklist_digest.as_deref(),
        rt.operational.has_checklist_evidence(),
    ) && rt.operational.saved.pending_checklists.is_empty()
        && rt.operational.saved.qualification.launch_qualified()
        && rt.operational.persist_error.is_none()
        && rt.operational.topology_stable
        && rt.operational.saved.fill_target_kg == Some(target)
        && rt
            .fresh_mass(now)
            .is_some_and(|mass| fill_mass_in_band(&rt.engine.config, mass.value, target))
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
    rt.fill_trim = None;
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
                .is_some_and(|mass| fill_mass_in_band(&rt.engine.config, mass.value, target))
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
/// Publish the durable mission stage through the existing real FC state path.
/// G_Armed asks FC to perform Postinit; it never bypasses FC readiness checks.
pub(super) fn publish_qualified_flight_armed(state: &Arc<AppState>) {
    if cfg!(feature = "test_fire_mode") || !prelaunch(state) {
        return;
    }
    let qualified = {
        let rt = state.gse.lock().unwrap();
        rt.operational.saved.qualification.stage()
            == gse_sequence::qualification::MissionStage::Armed
            && rt.operational.persist_error.is_none()
            && rt.operational.saved.pending_checklists.is_empty()
            && matches!(
                rt.engine.status.phase,
                Phase::Paused | Phase::Passed | Phase::Idle
            )
    };
    if qualified {
        state.set_local_flight_state(FlightState::Armed);
    }
}
/// Returns true when automation must not advance this tick.
pub(super) fn operational_tick(state: &Arc<AppState>, interlock: bool) -> bool {
    let blocked = {
        let rt = state.gse.lock().unwrap();
        rt.operational.saved.qualification.stage()
            == gse_sequence::qualification::MissionStage::Fire
            || rt.operational.saved.automatic_abort_claimed
    };
    if blocked { return true; }
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
    if !rt.manual_control
        && !rt.operational.startup_configured
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
fn qd_dispatch_timestamp_ms() -> u64 {
    crate::telemetry_task::procedure_clock::now().utc_ms()
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
                rt.effects_queued = true;
                let now = rt.now();
                rt.operational.qd.dispatched(now);
                rt.operational.qd_dispatch_utc_ms = Some(qd_dispatch_timestamp_ms());
                drop(rt);
                state.set_pending_umbilical_valve_state(
                    crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8,
                    true,
                );
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
    if state.gse.lock().unwrap().operational.saved.automatic_abort_claimed { return; }
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
    let nitrous_session = rt.engine.nitrous_session()
        || matches!(
            phase,
            Phase::FillSetup | Phase::Filling | Phase::PauseSetup | Phase::Paused
        );
    let unexpected_supply = if nitrous_session && rt.valves[3].is_some_and(|v| v.open) {
        Some("Nitrogen opened during nitrous fill")
    } else if (!nitrous_session && rt.valves[3].is_some_and(|v| v.open)
        || matches!(phase, Phase::Raising | Phase::Settling | Phase::Holding))
        && rt.valves[4].is_some_and(|v| v.open)
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
    async fn trim_failure_uses_existing_emergency_relief_without_waiting_for_ack() {
        let state = fixture().await;
        {
            let mut rt = state.gse.lock().unwrap();
            let (controller, _) =
                gse_sequence::fill_trim::Controller::start(rt.now(), 5., 200., &rt.engine.config)
                    .unwrap();
            rt.fill_trim = Some(controller);
            rt.engine.status.phase = Phase::PauseSetup;
            rt.operational.running = true;
            rt.observe_mass(None);
        }
        tick(&state, true);
        assert_eq!(state.gse.lock().unwrap().engine.status.phase, Phase::Fault);
        for (key, requested) in KEYS.into_iter().zip(gse_sequence::RELIEVED) {
            assert_eq!(
                state.get_pending_umbilical_valve_state(key),
                Some(requested)
            );
        }
        assert!(state.gse.lock().unwrap().fill_trim.is_none());
        cleanup(&state);
    }
    #[tokio::test]
    async fn changed_plans_revoke_only_affected_qualification_without_outputs() {
        let state = fixture().await;
        let path = armed_fixture_for_test(&state, state.topology_router.get().unwrap());
        let mut rt = state.gse.lock().unwrap();
        // A representation-only editor round trip preserves the proven plan.
        let mut config = rt.engine.config.clone();
        config.nitrogen_steps = config.nitrogen_plan().unwrap();
        assert!(!nitrogen_configuration_changed(&rt.engine.config, &config));
        config.fill_settle_ms += 1000;
        invalidate_changed_configuration(&mut rt, &config).unwrap();
        let saved = operational::Saved::load(&path).unwrap();
        assert!(saved.qualification.completed(Milestone::SelfTest));
        assert!(saved.qualification.nitrogen_satisfied());
        assert!(!saved.qualification.completed(Milestone::FillTarget));
        rt.engine.config = config.clone();
        config.nitrogen_steps[0].hold_ms += 1000;
        invalidate_changed_configuration(&mut rt, &config).unwrap();
        let saved = operational::Saved::load(&path).unwrap();
        assert!(saved.qualification.completed(Milestone::SelfTest));
        assert!(!saved.qualification.nitrogen_satisfied());
        assert!(!saved.qualification.completed(Milestone::FillTarget));
        assert!(
            state
                .pending_umbilical_valve_states
                .lock()
                .unwrap()
                .is_empty()
        );
        drop(rt);
        cleanup(&state);
    }
    #[tokio::test]
    async fn manual_handoff_preserves_reports_and_nitrous_crossflow_guard() {
        let state = fixture().await;
        apply_cold_flow_test_prerequisites(&state);
        {
            let mut rt = state.gse.lock().unwrap();
            let input = rt.input(true, true);
            rt.engine.request(Action::StartFill, input).unwrap();
            rt.engine.status.phase = Phase::Filling;
            rt.observe_valve(10, true);
            rt.operational.running = true;
        }
        state.set_umbilical_valve_state(0, false);
        let mut policy = state.action_policy_snapshot();
        policy.controls.retain(|control| control.cmd != "Pilot");
        policy.controls.push(ActionControl {
            cmd: "Pilot".into(),
            enabled: true,
            blink: BlinkMode::None,
            actuated: Some(false),
        });
        state.set_action_policy(policy);
        assert!(
            state.is_command_allowed(&TelemetryCommand::Pilot),
            "manual override must be reachable during fill: gate={:?} policy={:?}",
            command_allowed(&state, &TelemetryCommand::Pilot),
            state.action_policy_snapshot()
        );
        let before = state.gse.lock().unwrap().valves;
        assert!(!handle_command(&state, &TelemetryCommand::Pilot));
        {
            let rt = state.gse.lock().unwrap();
            assert_eq!(rt.engine.status.phase, Phase::Idle);
            assert!(!rt.engine.claimed);
            assert!(rt.manual_control);
            assert!(!rt.operational.running);
            assert_eq!(
                rt.valves.map(|v| v.map(|v| (v.open, v.at_ms))),
                before.map(|v| v.map(|v| (v.open, v.at_ms)))
            );
            assert!(rt.engine.nitrous_session());
            assert!(rt.engine.status.nitrogen_passed);
            assert!(rt.engine.status.self_test_locked);
        }
        assert!(
            state
                .pending_umbilical_valve_states
                .lock()
                .unwrap()
                .is_empty(),
            "handoff must not issue CLOSED/RELIEVED"
        );
        assert_eq!(
            command_allowed(&state, &TelemetryCommand::Nitrogen),
            Some(false)
        );
        tick(&state, true);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Idle,
            "no startup or legacy takeover"
        );
        state.gse.lock().unwrap().observe_valve(9, true);
        report_safety(&state);
        assert_eq!(
            state.gse.lock().unwrap().engine.status.phase,
            Phase::Fault,
            "manual mode must retain crossflow protection"
        );
        cleanup(&state);
    }
    fn apply_cold_flow_test_prerequisites(state: &AppState) {
        let mut rt = state.gse.lock().unwrap();
        rt.engine.status.nitrogen_passed = true;
        rt.engine.status.self_test_locked = true;
        rt.engine.status.phase = Phase::Passed;
        rt.engine.status.baseline = Some(gse_sequence::Baseline {
            average_psi: 0.,
            min_psi: 0.,
            max_psi: 0.,
            noise_psi: 0.1,
            samples: 50,
        });
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
                    let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
                    let mass = if phase == Phase::Filling {
                        target + target.signum() * rt.engine.config.fill_overfill_kg
                    } else if matches!(phase, Phase::PauseSetup | Phase::Paused) {
                        target
                    } else {
                        0.0
                    };
                    rt.observe_mass(Some(mass));
                    if phase == Phase::Raising && valves[3] {
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
    #[tokio::test]
    async fn durable_armed_publication_is_mode_sensitive_and_once_only() {
        let state = fixture().await;
        let mut updates = state.state_tx.subscribe();
        publish_qualified_flight_armed(&state);
        assert!(
            updates.try_recv().is_err(),
            "Incomplete qualification must not arm FC"
        );
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
            rt.engine.status.phase = Phase::Paused;
            rt.operational.persist_error = Some("simulated durable write failure".into());
        }
        publish_qualified_flight_armed(&state);
        assert!(
            updates.try_recv().is_err(),
            "A failed durable save must not arm FC"
        );
        state.gse.lock().unwrap().operational.persist_error = None;
        publish_qualified_flight_armed(&state);
        if cfg!(feature = "test_fire_mode") {
            assert!(updates.try_recv().is_err());
            assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        } else {
            assert_eq!(updates.try_recv().unwrap().state, FlightState::Armed);
            publish_qualified_flight_armed(&state);
            assert!(
                updates.try_recv().is_err(),
                "Repeated ticks must not republish Armed"
            );
        }
        cleanup(&state);
    }
    #[cfg(feature = "firmware_sim")]
    #[test]
    fn qd_dispatch_uses_the_explicit_model_source_epoch_without_changing_wall_mode() {
        const NAME: &str = "gse::operational_policy::tests::qd_dispatch_uses_the_explicit_model_source_epoch_without_changing_wall_mode";
        if let Ok(mode) = std::env::var("GS_QD_CLOCK_CHILD") {
            if mode == "model" {
                assert_eq!(qd_dispatch_timestamp_ms(), 1_767_225_612_345);
                assert_eq!(
                    qd_dispatch_timestamp_ms(),
                    1_767_225_612_345,
                    "Paused model must not advance the dispatch watermark"
                );
            } else {
                let before = crate::telemetry_task::get_current_timestamp_ms();
                let dispatched = qd_dispatch_timestamp_ms();
                let after = crate::telemetry_task::get_current_timestamp_ms();
                assert!((before..=after).contains(&dispatched));
            }
            return;
        }
        let dir = std::env::temp_dir().join(format!("gse-qd-clock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clock");
        std::fs::write(&path, "12345").unwrap();
        for mode in ["model", "wall"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", NAME, "--test-threads=1"])
                .env("GS_QD_CLOCK_CHILD", mode)
                .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000");
            if mode == "model" {
                child.env("GS_SIM_CLOCK_FILE", &path);
            } else {
                child.env_remove("GS_SIM_CLOCK_FILE");
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(not(feature = "test_fire_mode"))]
    #[tokio::test]
    async fn flight_launch_waits_five_seconds_from_qd_ack_and_rechecks_live_gates() {
        const NAME: &str = "gse::operational_policy::tests::flight_launch_waits_five_seconds_from_qd_ack_and_rechecks_live_gates";
        if std::env::var_os("GS_QD_GATES_CHILD").is_none() {
            let clock =
                std::env::temp_dir().join(format!("gse-qd-gates-clock-{}", std::process::id()));
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", NAME, "--test-threads=1"])
                .env("GS_QD_GATES_CHILD", "1");
            if cfg!(feature = "firmware_sim") {
                std::fs::write(&clock, "0").unwrap();
                child
                    .env("GS_SIM_CLOCK_FILE", &clock)
                    .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000");
            } else {
                child.env_remove("GS_SIM_CLOCK_FILE");
            }
            let output = child.output().unwrap();
            if cfg!(feature = "firmware_sim") {
                std::fs::remove_file(clock).unwrap();
            }
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        for revoke in [
            "none",
            "gps",
            "fc-route",
            "five-second-valves",
            "ten-second-valves",
            "one-second-valves",
        ] {
            queued_flight_launch_scenario(revoke).await;
        }
    }
    #[cfg(not(feature = "test_fire_mode"))]
    async fn queued_flight_launch_scenario(revoke: &str) {
        let mut state = fixture().await;
        let (commands, mut received) = tokio::sync::mpsc::channel(8);
        Arc::get_mut(&mut state).unwrap().cmd_tx = commands;
        let mut notices = state.notifications_tx.subscribe();
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
        let mut seen = Vec::new();
        while let Ok(messages) = notices.try_recv() {
            seen.extend(messages);
        }
        assert!(
            seen.iter()
                .any(|notice| notice.message.contains("waiting for QD acknowledgement"))
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
        assert!(handle_operational_command(
            &state,
            &TelemetryCommand::Launch
        ));
        let mut seen = Vec::new();
        while let Ok(messages) = notices.try_recv() {
            seen.extend(messages);
        }
        assert!(
            seen.iter()
                .any(|notice| notice.message.contains("QD settling")
                    && notice.message.contains("seconds remaining"))
        );
        let exact_model = crate::simulation_clock_ms().is_some();
        let before = if exact_model { 4999 } else { 4900 };
        let ready = if exact_model { 5000 } else { 5100 };
        for (advance, should_launch) in [
            (before, false),
            (ready, matches!(revoke, "none" | "one-second-valves")),
        ] {
            if exact_model {
                let clock =
                    std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
                let next = clock.with_extension("next");
                std::fs::write(&next, (acknowledged_at + advance).to_string()).unwrap();
                std::fs::rename(next, clock).unwrap();
                for board in [
                    Board::GatewayBoard,
                    Board::ValveBoard,
                    Board::ActuatorBoard,
                    Board::DaqBoard,
                    Board::FlightComputer,
                ] {
                    state.mark_board_seen(
                        board.sender_id(),
                        crate::telemetry_task::get_current_timestamp_ms(),
                    );
                }
            }
            if advance == ready {
                match revoke {
                    "gps" => crate::antenna::clear_rocket_fix_for_test(),
                    "fc-route" => {
                        router.remove_side(radio).unwrap();
                    }
                    _ => (),
                }
            }
            let mut rt = state.gse.lock().unwrap();
            rt.clock = Instant::now() - std::time::Duration::from_millis(acknowledged_at + advance);
            rt.observe_pressure(Some(0.0));
            rt.observe_mass(Some(target));
            for key in KEYS {
                rt.observe_valve(key, false);
            }
            if matches!(
                revoke,
                "five-second-valves" | "ten-second-valves" | "one-second-valves"
            ) {
                let period = match revoke {
                    "five-second-valves" => 5000,
                    "ten-second-valves" => 10000,
                    _ => 1000,
                };
                // Periodic closed reports begin at t=1000. At QD Ready t=5000,
                // old5s/10s cadences have only t=1000 evidence;1Hz has t=5000.
                let now = rt.now();
                let stamp = 1000 + now.saturating_sub(1000) / period * period;
                for valve in rt.valves.iter_mut().flatten() {
                    valve.at_ms = stamp;
                }
            }
            drop(rt);
            operational_tick(&state, true);
            if should_launch {
                assert!(matches!(received.try_recv(), Ok(TelemetryCommand::Launch)));
            } else {
                assert!(received.try_recv().is_err());
            }
        }
        if !matches!(revoke, "none" | "one-second-valves") {
            assert!(
                !state.gse.lock().unwrap().operational.qd.queued(),
                "Lost live prerequisites must cancel the queued launch"
            );
            assert_ne!(
                state
                    .gse
                    .lock()
                    .unwrap()
                    .operational
                    .saved
                    .qualification
                    .stage(),
                gse_sequence::qualification::MissionStage::Fire
            );
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
        assert!(
            crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::AutomaticAbort)
                .is_none(),
            "Delayed automatic abort must stay blocked after durable Fire even if local state restarted Idle"
        );
        assert!(
            matches!(
                crate::telemetry_task::command_at_dispatch(&state, TelemetryCommand::Abort),
                Some(TelemetryCommand::Abort)
            ),
            "Manual abort authority remains explicit"
        );
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
