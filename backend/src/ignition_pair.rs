//! Admission-only readiness for the two physical ignition owners.
//! Route inspection is an observed snapshot, never an atomic physical-link guarantee.
use crate::{
    sequences::{ActionPolicyMsg, BlinkMode},
    state::AppState,
    types::TelemetryCommand,
};
use std::time::Duration;

pub(crate) mod observer;
pub(crate) const REQUIRED_MASK: u8 = 0x18; // Existing application board order: AB, VB.
const ROUTE_MAX_AGE_MS: u64 = 12_000; // Same bound as dedicated recovery endpoint verification.
pub(crate) const PAIR_PROOF_REFUSAL: &str = "Ignition requires fresh non-Abort Actuator and Valve Board readbacks";
/// A Launch saves its intent durably (journal, qualification, checklists)
/// between its last up-front check and the SDK admission that checks the same
/// proofs again. The up-front check asks for proofs that outlast that write:
/// one that expired during it would lock Launch with nothing sent.
pub(crate) const LAUNCH_COMMIT_ALLOWANCE: Duration = Duration::from_secs(2);

pub(crate) fn command_requires_pair(command: &TelemetryCommand) -> bool {
    matches!(
        crate::sequences::command_name(command),
        "IgniterSequence" | "Launch" | "GroundStationLaunch"
    )
}

pub(crate) fn packet_requires_pair(kind: sedsnet::config::DataType, payload: &[u8]) -> bool {
    let Some(command) = payload.first().map(|value| value & 0x7f) else {
        return false;
    };
    (kind == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
        && command == crate::rocket_commands::ActuatorBoardCommands::IgniterSequence as u8)
        || (kind == crate::telemetry_schema::data_type("VALVE_COMMAND")
            && command == crate::rocket_commands::ValveBoardCommands::Sequence as u8)
        || (kind == crate::telemetry_schema::data_type("FLIGHT_COMMAND")
            && command == crate::rocket_commands::FlightComputerCommands::Launch as u8)
}

/// Nonwaiting, observation-only read. The worker captures SDK state separately.
pub(crate) fn capture_route_token(state: &AppState) -> Result<observer::Token, &'static str> {
    if state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err("Ground station is shutting down");
    }
    if crate::runtime_mode::is_testing()
        && crate::flight_sim::sim_mode_enabled()
        && !crate::testing_board_lifecycle::enabled()
    {
        return Err("Ignition requires both selected virtual board links");
    }
    let router = state
        .topology_router
        .get()
        .ok_or("Ignition router unavailable")?;
    let token = state
        .ignition_observer
        .read(router, crate::gse::automatic_generation(state))?;
    #[cfg(test)]
    route_test_pause::after_snapshot(state);
    Ok(token)
}
pub(crate) fn require_routes(state: &AppState) -> Result<(), &'static str> {
    capture_route_token(state).map(|_| ())
}
pub(crate) fn same_router(state: &AppState, router: &sedsnet::router::Router) -> bool {
    state
        .topology_router
        .get()
        .is_some_and(|bound| std::ptr::eq(bound.as_ref(), router))
}
/// RAM/atomic-only final validation; callers may hold Stop and board owners.
pub(crate) fn validate_route_token(
    state: &AppState,
    token: observer::Token,
) -> Result<(), &'static str> {
    if crate::runtime_mode::is_testing()
        && crate::flight_sim::sim_mode_enabled()
        && !crate::testing_board_lifecycle::selected_links_present()
    {
        return Err("Ignition virtual board selection changed before SDK admission");
    }
    let router = state
        .topology_router
        .get()
        .ok_or("Ignition router unavailable")?;
    if !state
        .shutdown_requested
        .load(std::sync::atomic::Ordering::Acquire)
        && state
            .ignition_observer
            .valid(&token, router, crate::gse::automatic_generation(state))
    {
        Ok(())
    } else {
        Err("Ignition route observation changed before SDK admission")
    }
}

/// HITL's board-state proof switch is opt-in; normal builds always enforce it.
// Safety catalog (About safety): ignition-pair. Keep backend/src/safety_catalog_rules.rs in step.
pub(crate) fn proof_enforced(state: &AppState) -> bool {
    state.board_state_proof_enforced()
}
/// Pair members whose proof is required. HITL runs with whichever boards are
/// on the bench, so an absent Actuator or Valve Board is left out; every other
/// mode requires both.
pub(crate) fn required_pair(state: &AppState) -> u8 {
    if crate::runtime_mode::is_hitl() {
        REQUIRED_MASK & crate::board_abort::present_mask(state)
    } else {
        REQUIRED_MASK
    }
}
/// The pair proof still fresh `ahead` from now and (with both boards present)
/// their routes.
fn pair_proof(state: &AppState, check_routes: bool, ahead: Duration) -> Result<(), &'static str> {
    let required = required_pair(state);
    if !state.board_abort.ignition_pair_current_ahead(required, ahead) {
        return Err(PAIR_PROOF_REFUSAL);
    }
    if !check_routes || required != REQUIRED_MASK {
        return Ok(());
    }
    require_routes(state)
}

pub(crate) fn readiness(state: &AppState) -> Result<(), &'static str> {
    readiness_ahead(state, Duration::ZERO)
}
/// `readiness` for a Launch, with a proof that outlasts saving its intent.
pub(crate) fn launch_readiness(state: &AppState) -> Result<(), &'static str> {
    readiness_ahead(state, LAUNCH_COMMIT_ALLOWANCE)
}
/// The readiness a command that requires the pair must have.
pub(crate) fn command_readiness(state: &AppState, command: &TelemetryCommand) -> Result<(), &'static str> {
    if launch_control(crate::sequences::command_name(command)) {
        launch_readiness(state)
    } else {
        readiness(state)
    }
}
fn launch_control(name: &str) -> bool {
    matches!(name, "Launch" | "GroundStationLaunch")
}
fn readiness_ahead(state: &AppState, ahead: Duration) -> Result<(), &'static str> {
    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return Err("Ground station is shutting down");
    }
    if state.board_abort.launch_abort_mask(state) != 0 {
        return Err("A board reports Abort; ignition is inhibited");
    }
    if state.topology_router.get().is_none() {
        return Err("Ignition router unavailable");
    }
    if !proof_enforced(state) {
        return Ok(());
    }
    pair_proof(state, true, ahead).inspect_err(|reason| {
        if *reason == PAIR_PROOF_REFUSAL {
            state
                .board_abort
                .request_queries(required_pair(state) | crate::board_abort::required_mask(state));
        }
    })
}

/// The ignition controls show what `command_readiness` admits, and why not.
fn project(state: &AppState, policy: &mut ActionPolicyMsg, check_routes: bool) {
    if crate::rrls::is_rrls() {
        return;
    }
    let blocked = state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire)
        || state.board_abort.launch_abort_mask(state) != 0;
    let proof = |ahead| proof_enforced(state).then(|| pair_proof(state, check_routes, ahead).err()).flatten();
    let (sequence, launch) = (proof(Duration::ZERO), proof(LAUNCH_COMMIT_ALLOWANCE));
    for control in &mut policy.controls {
        let reason = match control.cmd.as_str() {
            "IgniterSequence" => sequence,
            name if launch_control(name) => launch,
            _ => continue,
        };
        if !blocked && reason.is_none() {
            continue;
        }
        if control.enabled && control.disabled_reason.is_none() {
            control.disabled_reason = reason.map(str::to_string);
        }
        control.enabled = false;
        control.blink = BlinkMode::None;
    }
}

/// Existing policy refresh calls this outside Dispatch. Never require RRLS-local
/// fill routes: RRLS projects the primary's policy and commands return to primary.
pub(crate) fn project_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    project(state, policy, true);
}

/// Snapshot callers can already hold application owners: inspect RAM only here.
pub(crate) fn project_fresh_proof(state: &AppState, policy: &mut ActionPolicyMsg) {
    project(state, policy, false);
}

#[cfg(test)]
mod tests {
    use super::*;
    /// HITL runs the launch sequence with whichever boards are on the bench:
    /// a board that is not on the network is never required for progression
    /// or for the ignition pair proof. Other modes keep requiring them.
    #[tokio::test]
    async fn hitl_requires_only_the_boards_on_the_bench() {
        use crate::types::Board;
        use std::sync::atomic::Ordering;
        let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
        let state = crate::state::tests::test_app_state().await;
        state.av_bay_comms_connected.store(true, Ordering::Relaxed);
        state.fill_comms_connected.store(true, Ordering::Relaxed);
        state.mark_board_seen("VB", crate::telemetry_task::get_current_timestamp_ms());
        assert!(state.board_required_for_progression(Board::ValveBoard));
        for absent in [Board::ActuatorBoard, Board::GatewayBoard, Board::DaqBoard, Board::FlightComputer, Board::RFBoard, Board::PowerBoard] {
            assert!(!state.board_required_for_progression(absent), "{absent:?} is not on the bench");
        }
        assert!(state.all_required_boards_seen());
        assert_eq!(required_pair(&state), 1 << 4, "only the Valve Board of the pair is present");
        drop(_mode);
        let _normal = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Normal);
        assert_eq!(required_pair(&state), REQUIRED_MASK);
    }

    /// The Launch controls follow the Launch admission, which needs a proof
    /// that outlasts saving the intent; the igniter sequence saves nothing.
    /// A control the proof disables says so.
    #[tokio::test]
    async fn launch_controls_follow_the_launch_proof_window() {
        use crate::types::Board;
        // HITL's controls include the igniter sequence; its proof is opt-in.
        let _mode = crate::runtime_mode::test_support::thread_mode(crate::station_mode::Mode::Hitl);
        let state = crate::state::tests::test_app_state().await;
        state.toggle_hitl_safety_interlock(3);
        for board in ["AB", "VB"] {
            state.mark_board_seen(board, crate::telemetry_task::get_current_timestamp_ms());
        }
        let control = |age: Duration, name: &str| {
            for board in [Board::ActuatorBoard, Board::ValveBoard] {
                let now = std::time::Instant::now();
                state.board_abort.observe_status(board, 42, 8, 1, Some(1), false, true,
                    now - age, state.board_abort.query_revision(board), now, 1);
            }
            let mut policy = crate::sequences::default_action_policy();
            for control in &mut policy.controls {
                control.enabled = true;
                control.disabled_reason = None;
            }
            project_fresh_proof(&state, &mut policy);
            policy.controls.iter().find(|c| c.cmd == name).map(|c| (c.enabled, c.disabled_reason.clone()))
        };
        let refused = Some((false, Some(PAIR_PROOF_REFUSAL.to_string())));
        let enabled = Some((true, None));
        let ttl = crate::board_abort::PROOF_TTL;
        assert_eq!(control(Duration::ZERO, "Launch"), enabled);
        assert_eq!(control(ttl - Duration::from_secs(1), "Launch"), refused, "would lapse before SDK admission");
        assert_eq!(control(ttl - Duration::from_secs(1), "IgniterSequence"), enabled);
        assert_eq!(control(ttl + Duration::from_secs(1), "Launch"), refused);
        assert_eq!(control(ttl + Duration::from_secs(1), "IgniterSequence"), refused);
    }

    #[tokio::test]
    async fn hitl_proof_opt_in_preserves_actual_abort_stop_shutdown_and_router_identity() {
        if crate::operator_queue::isolated_native_case(module_path!(), "hitl_proof_opt_in_preserves_actual_abort_stop_shutdown_and_router_identity") { return; }
        for blocker in ["none", "proof_on", "abort", "stop", "shutdown", "other_router"] {
            let state = crate::state::tests::test_app_state().await;
            let router = std::sync::Arc::new(sedsnet::router::Router::new(
                sedsnet::router::RouterConfig::new([]).with_sender("GS")));
            state.topology_router.set(router.clone()).unwrap();
            router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
            if crate::runtime_mode::is_hitl() && blocker == "proof_on" {
                state.toggle_hitl_safety_interlock(3);
                // HITL requires the proof only from pair boards on the bench.
                state.mark_board_seen("AB", crate::telemetry_task::get_current_timestamp_ms());
            }
            if blocker == "abort" {
                state.board_abort.observe_abort(crate::types::Board::ActuatorBoard, std::time::Instant::now(), 100);
            }
            if blocker == "stop" { state.manual_abort_intent.request_untimed(); }
            if blocker == "shutdown" { state.request_shutdown(); }
            let unrelated = sedsnet::router::Router::new(sedsnet::router::RouterConfig::new([]).with_sender("OTHER"));
            let dispatch_router = if blocker == "other_router" { &unrelated } else { router.as_ref() };
            let result = crate::gse::queue_controller_with_stop_fence(
                &state, dispatch_router, crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                &[crate::rocket_commands::ActuatorBoardCommands::IgniterSequence as u8]);
            let accepted = result.unwrap_or(false);
            let expected = crate::runtime_mode::is_hitl() && blocker == "none";
            assert_eq!(accepted, expected, "blocker={blocker}, optional proof must not bypass actual Abort/Stop/identity");
            if blocker == "none" {
                if crate::runtime_mode::is_hitl() {
                    for index in 0..4 { assert!(!state.hitl_safety_interlock_enabled(index)); }
                    state.set_action_policy(crate::sequences::default_action_policy());
                    let policy = state.action_policy_snapshot();
                    for name in ["TogglePlumbingDelayInterlock", "ToggleChecklistInterlock", "ToggleSupplyInterlock", "ToggleBoardStateProofInterlock"] {
                        assert_eq!(policy.controls.iter().find(|control|control.cmd==name).unwrap().actuated, Some(false), "public policy default {name}");
                    }
                }
                assert_eq!(proof_enforced(&state), !crate::runtime_mode::is_hitl());
                assert_eq!(readiness(&state).is_ok(), crate::runtime_mode::is_hitl());
            }
            if blocker == "proof_on" || blocker == "abort" || blocker == "shutdown" {
                assert!(readiness(&state).is_err());
            }
            state.request_shutdown();
        }
    }

    #[test]
    fn automatic_bit_does_not_bypass_sequence_classification_and_off_is_not_gated() {
        crate::telemetry_schema::initialize().unwrap();
        for (name, command) in [
            (
                "ACTUATOR_COMMAND",
                crate::rocket_commands::ActuatorBoardCommands::IgniterSequence as u8,
            ),
            (
                "VALVE_COMMAND",
                crate::rocket_commands::ValveBoardCommands::Sequence as u8,
            ),
        ] {
            let kind = crate::telemetry_schema::data_type(name);
            for value in [command, command | 0x80] {
                assert!(packet_requires_pair(kind, &[value]));
            }
        }
        let kind = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
        assert!(!packet_requires_pair(
            kind,
            &[crate::rocket_commands::ActuatorBoardCommands::IgniterOff as u8]
        ));
        assert!(!packet_requires_pair(kind, &[]));
    }
}

#[cfg(test)]
pub(crate) mod route_test_pause {
    use super::*;
    use std::sync::{Arc, Mutex, OnceLock, mpsc};
    use std::time::Duration;
    struct Gate {
        observed: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    }
    struct Installed {
        key: usize,
        gate: Arc<Gate>,
    }
    static GATE: OnceLock<Mutex<Option<Installed>>> = OnceLock::new();
    pub(crate) struct Pause {
        pub(crate) observed: mpsc::Receiver<()>,
        pub(crate) resume: mpsc::Sender<()>,
        key: usize,
    }
    impl Pause {
        pub(crate) fn install(state: &AppState) -> Self {
            let (observed, receive) = mpsc::channel();
            let (resume, resumed) = mpsc::channel();
            let key = state as *const AppState as usize;
            *GATE.get_or_init(Default::default).lock().unwrap() = Some(Installed {
                key,
                gate: Arc::new(Gate {
                    observed,
                    resume: Mutex::new(resumed),
                }),
            });
            Self {
                observed: receive,
                resume,
                key,
            }
        }
    }
    impl Drop for Pause {
        fn drop(&mut self) {
            let _ = self.resume.send(());
            let mut gate = GATE.get().unwrap().lock().unwrap();
            if gate
                .as_ref()
                .is_some_and(|installed| installed.key == self.key)
            {
                *gate = None;
            }
        }
    }
    pub(super) fn after_snapshot(state: &AppState) {
        if std::thread::current().name() != Some("ignition-pair-new-control") {
            return;
        }
        let gate = GATE.get().and_then(|slot| {
            slot.lock()
                .unwrap()
                .as_ref()
                .filter(|installed| installed.key == state as *const AppState as usize)
                .map(|installed| installed.gate.clone())
        });
        if let Some(gate) = gate {
            gate.observed.send(()).unwrap();
            gate.resume
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        }
    }
}

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
pub(crate) mod constrained_fixture;
