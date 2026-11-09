//! Original, non-serializable operator phase intent. Local metadata is not Fire.
use crate::{
    state::AppState,
    types::{FlightState, TelemetryCommand},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const ORDER: [FlightState; 7] = [
    FlightState::Startup,
    FlightState::Idle,
    FlightState::PreFill,
    FlightState::FillTest,
    FlightState::NitrogenFill,
    FlightState::NitrousFill,
    FlightState::Armed,
];
#[derive(Debug, Clone)]
pub(crate) struct Intent {
    token: crate::operator_queue::Token,
    ticket: crate::gse::LaunchAdmission,
    expected: FlightState,
    target: Option<FlightState>,
    revision: u64,
    local: bool,
    forward: bool,
    captured: Instant,
}
impl Intent {
    pub(crate) fn name(&self) -> &'static str {
        if self.forward {
            "AdvanceFlightState"
        } else {
            "RewindFlightState"
        }
    }
    pub(crate) fn local(&self) -> bool {
        self.local
    }
    pub(crate) fn ticket(&self) -> crate::gse::LaunchAdmission {
        self.ticket
    }
    pub(crate) fn context(&self) -> (FlightState, Option<FlightState>, u64, bool) {
        (self.expected, self.target, self.revision, self.local)
    }
    /// Bind this immutable request to its own admitted manual receipt. Only
    /// the relief fence advances; deadline, requested phase and Stop stay sealed.
    pub(crate) fn bind_manual_claim(
        &mut self,
        state: &AppState,
        origin: &crate::operator_queue::manual_origin::Origin,
    ) -> bool {
        let Some(successor) = origin.phase_successor(state, self.token) else { return false; };
        self.token.relief_epoch = successor;
        true
    }
    pub(crate) fn validate(&self, state: &AppState) -> bool {
        self.revision != u64::MAX
            && self.captured.elapsed() <= Duration::from_secs(5)
            && self.token.generation == crate::gse::automatic_generation(state)
            && self.token.manual_epoch == state.manual_abort_intent.epoch()
            && self.token.observed_manual_epoch == state.deferred_abort.manual_epoch()
            && self.token.relief_epoch == state.operator_queue.relief_epoch()
            && crate::gse::operator_phase_mode(state).is_some_and(|(enabled, revision)| {
                enabled == self.local && revision == self.revision
            })
            && state.local_flight_state_snapshot() == self.expected
            && (!self.local || self.target.is_some() && local_allowed(state))
    }
    pub(crate) fn original_command(&self) -> TelemetryCommand {
        if self.forward {
            TelemetryCommand::AdvanceFlightState
        } else {
            TelemetryCommand::RewindFlightState
        }
    }
}
pub(crate) fn local_mode(state: &AppState) -> bool {
    crate::runtime_mode::is_hitl()
        && !crate::rrls::is_rrls()
        && state
            .phase_control_selected
            .load(std::sync::atomic::Ordering::Acquire)
}

fn adjacent(current: FlightState, forward: bool) -> Option<FlightState> {
    let at = ORDER.iter().position(|s| *s == current)?;
    if forward {
        ORDER.get(at + 1).copied()
    } else {
        at.checked_sub(1).and_then(|at| ORDER.get(at).copied())
    }
}
/// A busy mode owner is uncertainty, never authority for a delayed FC phase.
pub(crate) fn fc_prelaunch_is_raw(state: &AppState) -> bool {
    local_mode(state)
}

pub(crate) fn local_allowed(state: &AppState) -> bool {
    local_mode(state)
        && !state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        && !state.abort_indicator_latched()
        && crate::gse::manual_stop_state(state) == crate::gse::CurrentState::Current
        && state.board_abort.masks().0 == 0
        && crate::gse::operator_phase_runtime_available(state)
}
/// Plain-language reason a phase step is refused, checked in admission order.
pub(crate) fn refusal_reason(state: &AppState, forward: bool) -> String {
    if !local_mode(state) {
        return "Phase steps need HITL local phase control selected on the primary station.".into();
    }
    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return "The station is shutting down.".into();
    }
    if state.abort_indicator_latched() {
        return "An Abort indicator is latched. Use Reset all boards first.".into();
    }
    if crate::gse::manual_stop_state(state) != crate::gse::CurrentState::Current {
        return "A manual Stop is not yet confirmed by the boards.".into();
    }
    if state.board_abort.masks().0 != 0 {
        return "A board reports Abort. Recover it or use Reset all boards first.".into();
    }
    if !crate::gse::operator_phase_runtime_available(state) {
        return "Cold flow is running or the GSE runtime is busy; try again.".into();
    }
    if !button_allowed(state) {
        return "The HITL button interlock is not satisfied.".into();
    }
    if !controls_allowed(state) {
        return "GroundStation key or software controls are disabled (or the policy was updating); try again.".into();
    }
    if adjacent(state.local_flight_state_snapshot(), forward).is_none() {
        return if forward { "There is no later flight state to advance to.".into() } else { "There is no earlier flight state to rewind to.".into() };
    }
    "The request expired or the flight state changed before it was sent. Press it again.".into()
}
pub(crate) fn enabled_for_name(state: &AppState, name: &str) -> Option<bool> {
    if !local_mode(state) {
        return None;
    }
    let forward = match name {
        "AdvanceFlightState" => true,
        "RewindFlightState" => false,
        _ => return None,
    };
    Some(
        local_allowed(state)
            && button_allowed(state)
            && controls_allowed(state)
            && adjacent(state.local_flight_state_snapshot(), forward).is_some(),
    )
}
pub(crate) fn is_local(command: &TelemetryCommand) -> bool {
    matches!(command,TelemetryCommand::OperatorPhase{intent} if intent.local())
}
pub(crate) fn seal(state: &AppState, command: TelemetryCommand) -> TelemetryCommand {
    if crate::runtime_mode::is_hitl() {
        let forward = match command {
            TelemetryCommand::AdvanceFlightState => true,
            TelemetryCommand::RewindFlightState => false,
            _ => return command,
        };
        if crate::rrls::is_rrls() {
            return command;
        }
        let (local, revision) = crate::gse::operator_phase_mode(state).unwrap_or((true, u64::MAX));
        let token = crate::operator_queue::Token::capture(state, 0);
        let expected = state
            .state
            .try_lock()
            .ok()
            .map(|value| *value)
            .unwrap_or(FlightState::Aborted);
        TelemetryCommand::OperatorPhase {
            intent: Intent {
                ticket: crate::gse::operator_phase_ticket(state, token),
                token,
                expected,
                target: adjacent(expected, forward),
                revision,
                local,
                forward,
                captured: Instant::now(),
            },
        }
    } else {
        command
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublicBinding {
    phase: FlightState,
    revision: u64,
    generation: u64,
    manual: u64,
    observed: u64,
    board_revision: u64,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicSnapshot {
    binding: PublicBinding,
    mask: u8,
}
pub(crate) fn project(state: &AppState, policy: &mut crate::sequences::ActionPolicyMsg) {
    policy.local_phase_controls = local_mode(state);
    policy.local_phase_controls_reason = None;
    if !policy.local_phase_controls {
        *state.phase_control_public.lock().unwrap() = None;
        return;
    }
    let phase = state.local_flight_state_snapshot();
    let binding = PublicBinding {
        phase,
        revision: state
            .phase_control_revision
            .load(std::sync::atomic::Ordering::Acquire),
        generation: crate::gse::automatic_generation(state),
        manual: state.manual_abort_intent.epoch(),
        observed: state.deferred_abort.manual_epoch(),
        board_revision: state.board_abort.abort_revision(),
    };
    let stop = crate::gse::manual_stop_state(state);
    let runtime = crate::gse::operator_phase_public_context(state);
    let deny = !policy.key_enabled
        || !policy.software_buttons_enabled
        || !button_allowed(state)
        || state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
        || state.abort_indicator_latched()
        || state.board_abort.masks().0 != 0
        || binding.revision == u64::MAX
        || binding.manual == u64::MAX
        || binding.observed == u64::MAX
        || matches!(stop, crate::gse::CurrentState::Invalid)
        || runtime.is_some_and(|(selected, revision, idle)| {
            !selected || revision != binding.revision || !idle
        });
    let cached = *state.phase_control_public.lock().unwrap();
    let mask = if deny {
        0
    } else if runtime == Some((true, binding.revision, true))
        && matches!(stop, crate::gse::CurrentState::Current)
    {
        u8::from(adjacent(phase, false).is_some())
            | (u8::from(adjacent(phase, true).is_some()) << 1)
    } else {
        cached
            .filter(|value| value.binding == binding)
            .map_or(0, |value| value.mask)
    };
    *state.phase_control_public.lock().unwrap() = Some(PublicSnapshot { binding, mask });
    for control in &mut policy.controls {
        let bit = match control.cmd.as_str() {
            "RewindFlightState" => 1,
            "AdvanceFlightState" => 2,
            _ => continue,
        };
        control.enabled = mask & bit != 0;
        control.blink = crate::sequences::BlinkMode::None;
    }
}

pub(crate) fn apply(state: &Arc<AppState>, intent: &Intent) -> Result<(), String> {
    if !intent.local()
        || !intent.validate(state)
        || !button_allowed(state)
        || !controls_allowed(state)
    {
        return Err("Phase request changed or expired; choose a new phase action.".into());
    }
    let target = intent
        .target
        .ok_or("Use explicit Fire, Abort or verified reset at this phase boundary.")?;
    let router = state
        .topology_router
        .get()
        .ok_or("Phase router unavailable")?;
    let packet = crate::network_variables::operator_phase_packet(router, target as u8)
        .map_err(|e| e.to_string())?;
    let mut prepared = router
        .prepare_network_variable_batch(
            vec![packet],
            crate::network_variables::automatic_idle_batch_limits(),
        )
        .map_err(|error| format!("Phase update could not be prepared: {:?}", error.error))?;
    crate::gse::commit_operator_phase(state, intent, &mut prepared)?;
    state.publish_operator_phase_observation();
    Ok(())
}

pub(crate) fn controls_allowed(state: &AppState) -> bool {
    state
        .action_policy
        .try_lock()
        .ok()
        .is_some_and(|policy| policy.key_enabled && policy.software_buttons_enabled)
}
pub(crate) fn button_allowed(state: &AppState) -> bool {
    if crate::runtime_mode::is_hitl() {
        state.hitl_button_interlock_satisfied()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn intent(state: &AppState, forward: bool) -> Intent {
        let cmd = seal(
            state,
            if forward {
                TelemetryCommand::AdvanceFlightState
            } else {
                TelemetryCommand::RewindFlightState
            },
        );
        match cmd {
            TelemetryCommand::OperatorPhase { intent } => intent,
            _ => panic!("sealed origin required"),
        }
    }
    #[tokio::test]
    async fn local_phase_http_permission_and_actual_managed_publication_need_no_fc_or_launch_proof()
    {
        crate::mode_suite!(Hitl);
        let mut fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        if fixture.state.hitl_button_interlock_enabled() {
            fixture.state.toggle_hitl_safety_interlock(0);
        }
        fixture
            .host
            .enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        assert!(!fixture.state.board_abort.ignition_pair_current());
        let denied = fixture
            .http(
                "POST",
                "/api/command",
                Some("viewer"),
                serde_json::json!("AdvanceFlightState"),
            )
            .await;
        assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);
        let policy = fixture.state.action_policy_snapshot();
        assert!(policy.local_phase_controls);
        assert!(
            policy
                .controls
                .iter()
                .find(|c| c.cmd == "AdvanceFlightState")
                .unwrap()
                .enabled
        );
        let response = fixture
            .http(
                "POST",
                "/api/command",
                Some("operator"),
                serde_json::json!("AdvanceFlightState"),
            )
            .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let queued = fixture.cmd_rx.as_mut().unwrap().recv().await.unwrap();
        let TelemetryCommand::QueuedOperator { command, .. } = queued else {
            panic!("ordinary operator queue required")
        };
        let TelemetryCommand::OperatorPhase { intent } = *command else {
            panic!("immutable local phase required")
        };
        assert!(intent.local());
        let mut updates = fixture.state.state_tx.subscribe();
        apply(&fixture.state, &intent).unwrap();
        assert_eq!(
            fixture.state.local_flight_state_snapshot(),
            FlightState::PreFill
        );
        assert_eq!(updates.try_recv().unwrap().state, FlightState::PreFill);
        fixture.pump();
        assert!(
            !fixture
                .host
                .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND"))
        );
        let cached = fixture
            .host
            .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        assert_eq!(cached.sender(), "GS");
        assert_eq!(cached.payload(), [FlightState::PreFill as u8]);
        assert!(
            apply(&fixture.state, &intent).is_err(),
            "old relative intent cannot retarget new phase"
        );
    }
    #[tokio::test]
    async fn boundaries_stop_and_control_mode_aba_refuse_old_intents() {
        crate::mode_suite!(Hitl);
        for phase in [
            FlightState::Startup,
            FlightState::Armed,
            FlightState::Launch,
            FlightState::Aborted,
        ] {
            let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
            fixture.state.set_preexisting_flight_state_for_test(phase);
            let value = intent(&fixture.state, phase != FlightState::Startup);
            assert!(
                apply(&fixture.state, &value).is_err(),
                "Fire/Abort/reset boundary must be explicit"
            );
        }
        let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        let value = intent(&fixture.state, true);
        crate::gse::handle_command_with_origins(
            &fixture.state,
            &TelemetryCommand::ToggleGroundStationControl,
            None,
            None,
        );
        crate::gse::handle_command_with_origins(
            &fixture.state,
            &TelemetryCommand::ToggleGroundStationControl,
            None,
            None,
        );
        assert!(
            !value.validate(&fixture.state),
            "mode ABA cannot resurrect intent"
        );
        let value = intent(&fixture.state, true);
        fixture.state.manual_abort_intent.request();
        assert!(apply(&fixture.state, &value).is_err());
        let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        let value = intent(&fixture.state, true);
        fixture
            .state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        // Retained automatic-launch bookkeeping is not a manual phase veto.
        assert!(value.validate(&fixture.state));
        fixture.state.operator_admission_barrier.store(77, std::sync::atomic::Ordering::Release);
        assert!(value.validate(&fixture.state));
        let policy = fixture.state.action_policy_snapshot();
        for name in ["AdvanceFlightState", "RewindFlightState"] {
            assert!(policy.controls.iter().find(|row| row.cmd == name).unwrap().enabled,
                "recovery bookkeeping must not disable fresh manual phase controls");
        }
    }
    #[test]
    fn external_json_cannot_forge_internal_phase_and_ladder_never_contains_fire() {
        crate::mode_suite!(Hitl);
        assert!(
            serde_json::from_value::<TelemetryCommand>(
                serde_json::json!({"OperatorPhase":{"intent":{}}})
            )
            .is_err()
        );
        assert_eq!(adjacent(FlightState::Startup, false), None);
        assert_eq!(adjacent(FlightState::Armed, true), None);
        assert_eq!(adjacent(FlightState::Aborted, false), None);
        assert_eq!(adjacent(FlightState::Launch, false), None);
    }
    #[tokio::test]
    async fn websocket_origin_and_old_fc_authority_are_not_retargeted() {
        crate::mode_suite!(Hitl);
        let mut fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        if fixture.state.hitl_button_interlock_enabled() {
            fixture.state.toggle_hitl_safety_interlock(0);
        }
        crate::web::handle_ws_control_text_with_source(
            fixture.state.clone(),
            Some("operator".into()),
            fixture.state.cmd_tx.clone(),
            serde_json::json!({"cmd":"AdvanceFlightState"}).to_string(),
            Arc::new(|| true),
        )
        .await;
        let queued = fixture.cmd_rx.as_mut().unwrap().recv().await.unwrap();
        let TelemetryCommand::QueuedOperator { command, .. } = queued else {
            panic!("normal queue required")
        };
        let TelemetryCommand::OperatorPhase { intent: local } = *command else {
            panic!("WS must seal before auth")
        };
        assert!(local.local());
        crate::gse::handle_command_with_origins(
            &fixture.state,
            &TelemetryCommand::ToggleGroundStationControl,
            None,
            None,
        );
        let fc = intent(&fixture.state, true);
        assert!(!fc.local());
        assert!(fc.validate(&fixture.state));
        crate::gse::handle_command_with_origins(
            &fixture.state,
            &TelemetryCommand::ToggleGroundStationControl,
            None,
            None,
        );
        assert!(
            !fc.validate(&fixture.state),
            "old FC relative command cannot become a new local relative command"
        );
        assert!(
            !local.validate(&fixture.state),
            "returning to original mode cannot resurrect old WS command"
        );
    }
    #[tokio::test]
    async fn expiry_phase_change_and_software_disable_fail_closed() {
        crate::mode_suite!(Hitl);
        let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        let mut old = intent(&fixture.state, true);
        old.captured -= Duration::from_secs(6);
        assert!(apply(&fixture.state, &old).is_err());
        let old = intent(&fixture.state, true);
        fixture
            .state
            .set_preexisting_flight_state_for_test(FlightState::Armed);
        assert!(apply(&fixture.state, &old).is_err());
        fixture
            .state
            .set_preexisting_flight_state_for_test(FlightState::Idle);
        let old = intent(&fixture.state, true);
        fixture
            .state
            .action_policy
            .lock()
            .unwrap()
            .software_buttons_enabled = false;
        assert!(apply(&fixture.state, &old).is_err());
        fixture
            .state
            .action_policy
            .lock()
            .unwrap()
            .software_buttons_enabled = true;
        fixture.state.action_policy.lock().unwrap().key_enabled = false;
        assert!(
            apply(&fixture.state, &old).is_err(),
            "key disable must survive local phase mode"
        );
    }
    #[tokio::test]
    async fn prepared_phase_refuses_new_stop_without_any_ram_ws_or_sdk_partial_update() {
        crate::mode_suite!(Hitl);
        let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        if fixture.state.hitl_button_interlock_enabled() {
            fixture.state.toggle_hitl_safety_interlock(0);
        }
        fixture
            .host
            .enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        let value = intent(&fixture.state, true);
        let packet = crate::network_variables::operator_phase_packet(
            &fixture.host,
            FlightState::PreFill as u8,
        )
        .unwrap();
        let mut prepared = fixture
            .host
            .prepare_network_variable_batch(
                vec![packet],
                crate::network_variables::automatic_idle_batch_limits(),
            )
            .unwrap();
        let mut events = fixture.state.state_tx.subscribe();
        fixture.state.manual_abort_intent.request();
        assert!(crate::gse::commit_operator_phase(&fixture.state, &value, &mut prepared).is_err());
        assert_eq!(
            fixture.state.local_flight_state_snapshot(),
            FlightState::Idle
        );
        assert!(events.try_recv().is_err());
        assert!(
            fixture
                .host
                .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .is_none()
        );
    }
    #[tokio::test]
    async fn actual_sdk_core_busy_refuses_prepared_phase_without_partial_publication() {
        crate::mode_suite!(Hitl);
        if crate::operator_queue::isolated_native_case(
            module_path!(),
            "actual_sdk_core_busy_refuses_prepared_phase_without_partial_publication",
        ) {
            return;
        }
        let state = crate::state::tests::test_app_state().await;
        if state.hitl_button_interlock_enabled() {
            state.toggle_hitl_safety_interlock(0);
        }
        state.set_preexisting_flight_state_for_test(FlightState::Idle);
        let owner = Arc::as_ptr(&state) as usize;
        let router = Arc::new(sedsnet::router::Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
            Box::new(move || {
                crate::gse::nonwaiting_admission_tests::clock_pause(owner);
                100
            }),
        ));
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_563_200_000,
            ))
            .unwrap();
        router
            .enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        state.topology_router.set(router.clone()).unwrap();
        let value = intent(&state, true);
        let packet =
            crate::network_variables::operator_phase_packet(&router, FlightState::PreFill as u8)
                .unwrap();
        let stamp = packet.timestamp();
        let mut prepared = router
            .prepare_network_variable_batch(
                vec![packet],
                crate::network_variables::automatic_idle_batch_limits(),
            )
            .unwrap();
        let seed = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            &[crate::telemetry_schema::endpoint("FLIGHT_STATE")],
            "GS",
            stamp.saturating_sub(1),
            Arc::from([1u8]),
        )
        .unwrap();
        let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&state);
        let host = router.clone();
        let holder = std::thread::Builder::new()
            .name("production-sdk-core-holder".into())
            .spawn(move || host.seed_managed_variable(seed))
            .unwrap();
        pause
            .core_entered
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(
                router.try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT),
                Err(sedsnet::router::TryPendingTypeError::CoreBusy)
            ),
            "actual public SDK Core must be held"
        );
        let mut events = state.state_tx.subscribe();
        assert!(crate::gse::commit_operator_phase(&state, &value, &mut prepared).is_err());
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        assert!(events.try_recv().is_err());
        pause.release_core();
        holder.join().unwrap().unwrap();
        assert_eq!(
            router
                .cached_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap()
                .payload(),
            [1]
        );
        assert!(events.try_recv().is_err());
    }
    #[tokio::test]
    async fn public_local_master_selection_survives_unchanged_runtime_contention() {
        crate::mode_suite!(Hitl);
        let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
        assert!(fixture.state.action_policy_snapshot().local_phase_controls);
        let value = intent(&fixture.state, true);
        let before = fixture.state.action_policy_snapshot();
        let _runtime = fixture.state.gse.lock().unwrap();
        let busy = fixture.state.action_policy_snapshot();
        assert!(
            busy.local_phase_controls,
            "unchanged local master selection must not become false while Runtime is busy"
        );
        for name in ["AdvanceFlightState", "RewindFlightState"] {
            assert_eq!(
                busy.controls
                    .iter()
                    .find(|row| row.cmd == name)
                    .unwrap()
                    .enabled,
                before
                    .controls
                    .iter()
                    .find(|row| row.cmd == name)
                    .unwrap()
                    .enabled,
                "public mask must retain unchanged committed eligibility"
            );
        }
        assert!(
            !value.validate(&fixture.state),
            "strict capture/admission still refuses busy Runtime"
        );
    }
}
