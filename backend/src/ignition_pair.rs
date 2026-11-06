//! Admission-only readiness for the two physical ignition owners.
//! Route inspection is an observed snapshot, never an atomic physical-link guarantee.
use crate::{
    sequences::{ActionPolicyMsg, BlinkMode},
    state::AppState,
    types::TelemetryCommand,
};

pub(crate) mod observer;
pub(crate) const REQUIRED_MASK: u8 = 0x18; // Existing application board order: AB, VB.
const ROUTE_MAX_AGE_MS: u64 = 12_000; // Same bound as dedicated recovery endpoint verification.

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
    #[cfg(all(
        feature = "testing",
        not(any(feature = "hitl_mode", feature = "test_fire_mode"))
    ))]
    if crate::flight_sim::sim_mode_enabled() && !crate::testing_board_lifecycle::enabled() {
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
    #[cfg(all(
        feature = "testing",
        not(any(feature = "hitl_mode", feature = "test_fire_mode"))
    ))]
    if crate::flight_sim::sim_mode_enabled()
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
pub(crate) fn proof_enforced(state: &AppState) -> bool {
    state.board_state_proof_enforced()
}

pub(crate) fn readiness(state: &AppState) -> Result<(), &'static str> {
    if state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
        return Err("Ground station is shutting down");
    }
    if state.board_abort.masks().0 != 0 {
        return Err("A board reports Abort; ignition is inhibited");
    }
    if state.topology_router.get().is_none() {
        return Err("Ignition router unavailable");
    }
    if !proof_enforced(state) {
        return Ok(());
    }
    if !state.board_abort.ignition_pair_current() {
        state
            .board_abort
            .request_queries(REQUIRED_MASK | crate::board_abort::required_mask(state));
        return Err("Ignition requires fresh non-Abort Actuator and Valve Board readbacks");
    }
    require_routes(state)
}

fn disable(policy: &mut ActionPolicyMsg) {
    for control in &mut policy.controls {
        if matches!(
            control.cmd.as_str(),
            "IgniterSequence" | "Launch" | "GroundStationLaunch"
        ) {
            control.enabled = false;
            control.blink = BlinkMode::None;
        }
    }
}

/// Existing policy refresh calls this outside Dispatch. Never require RRLS-local
/// fill routes: RRLS projects the primary's policy and commands return to primary.
pub(crate) fn project_policy(state: &AppState, policy: &mut ActionPolicyMsg) {
    if !crate::rrls::is_rrls()
        && (state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire)
            || state.board_abort.masks().0 != 0
            || proof_enforced(state) && (!state.board_abort.ignition_pair_current() || require_routes(state).is_err()))
    {
        disable(policy);
    }
}

/// Snapshot callers can already hold application owners: inspect RAM only here.
pub(crate) fn project_fresh_proof(state: &AppState, policy: &mut ActionPolicyMsg) {
    if !crate::rrls::is_rrls()
        && (state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire)
            || state.board_abort.masks().0 != 0
            || proof_enforced(state) && !state.board_abort.ignition_pair_current()) {
        disable(policy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn hitl_proof_opt_in_preserves_actual_abort_stop_shutdown_and_router_identity() {
        if crate::operator_queue::isolated_native_case(module_path!(), "hitl_proof_opt_in_preserves_actual_abort_stop_shutdown_and_router_identity") { return; }
        for blocker in ["none", "proof_on", "abort", "stop", "shutdown", "other_router"] {
            let state = crate::state::tests::test_app_state().await;
            let router = std::sync::Arc::new(sedsnet::router::Router::new(
                sedsnet::router::RouterConfig::new([]).with_sender("GS")));
            state.topology_router.set(router.clone()).unwrap();
            router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
            #[cfg(feature = "hitl_mode")]
            if blocker == "proof_on" { state.toggle_hitl_safety_interlock(3); }
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
            let expected = cfg!(feature = "hitl_mode") && blocker == "none";
            assert_eq!(accepted, expected, "blocker={blocker}, optional proof must not bypass actual Abort/Stop/identity");
            if blocker == "none" {
                #[cfg(feature = "hitl_mode")]
                {
                    for index in 0..4 { assert!(!state.hitl_safety_interlock_enabled(index)); }
                    state.set_action_policy(crate::sequences::default_action_policy());
                    let policy = state.action_policy_snapshot();
                    for name in ["TogglePlumbingDelayInterlock", "ToggleChecklistInterlock", "ToggleSupplyInterlock", "ToggleBoardStateProofInterlock"] {
                        assert_eq!(policy.controls.iter().find(|control|control.cmd==name).unwrap().actuated, Some(false), "public policy default {name}");
                    }
                }
                assert_eq!(proof_enforced(&state), !cfg!(feature = "hitl_mode"));
                assert_eq!(readiness(&state).is_ok(), cfg!(feature = "hitl_mode"));
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
