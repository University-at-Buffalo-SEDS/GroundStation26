// Actual persisted mission qualification + automatic public SDK projection.
use super::*;
use crate::automatic_flight_state::{Cause, Observation};
#[derive(Debug, PartialEq, Eq)]
struct CachedIdentity {
    id: u64, utc: u64, data_type: u32, sender: String,
    endpoints: Vec<sedsnet::config::DataEndpoint>, payload: Vec<u8>,
}
fn cached_identity(state: &Arc<AppState>) -> Option<CachedIdentity> {
    state.topology_router.get().unwrap()
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap().map(|packet| CachedIdentity {
            id: packet.packet_id(), utc: packet.timestamp(), data_type: packet.data_type().as_u32(),
            sender: packet.sender().to_string(), endpoints: packet.endpoints().to_vec(), payload: packet.payload().to_vec(),
        })
}
async fn qualified() -> Arc<AppState> {
    let (state, _) = crate::automatic_flight_state::tests::ready().await;
    crate::gse::initialize(&state);
    if !crate::runtime_mode::is_hitl_or_test_fire() {
        // Explicit independent durable fixture authority, NOT readiness,
        // operator permission, real board or sensor qualification evidence.
        // The existing Normal qualified projector requires Idle..Armed.
        state.set_local_flight_state(FlightState::Idle);
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
        assert_eq!(crate::network_variables::flight_state(), FlightState::Idle as u8);
        assert_eq!(cached_identity(&state).unwrap().payload, vec![FlightState::Idle as u8]);
    }
    // Match the existing operational-policy unit fixture's VALID saved model
    // prerequisites, not physical nitrogen/fill/board qualification evidence.
    let baseline = gse_sequence::Baseline {
        average_psi: 0.0, min_psi: 0.0, max_psi: 0.0, noise_psi: 0.1, samples: 50,
    };
    let fill_target = state.fill_targets_snapshot().nitrous.target_mass_kg;
    assert!(fill_target.is_finite() && fill_target != 0.0);
    {
        let mut rt = state.gse.lock().unwrap();
        rt.operational.saved.baseline = Some(baseline.clone());
        rt.engine.status.baseline = Some(baseline);
        rt.operational.saved.fill_target_kg = Some(fill_target);
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
        rt.engine.claimed = true; // original valid qualified owner, not legacy unclaimed
        rt.engine.status.phase = Phase::Paused;
        rt.operational.persist().unwrap();
    }
    state
}
#[tokio::test]
async fn qualified_claimed_owner_can_project_armed_live_without_changing_desired_json() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "qualified_claimed_owner_can_project_armed_live_without_changing_desired_json",
    ) {
        return;
    }
    let state = qualified().await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let observation = Observation::begin(&state).unwrap();
    publish_qualified_flight_armed_with_observation(&state, &observation);
    if crate::runtime_mode::is_test_fire() {
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    } else {
        assert_eq!(state.local_flight_state_snapshot(), FlightState::Armed);
        assert_eq!(
            state
                .topology_router
                .get()
                .unwrap()
                .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap()
                .unwrap()
                .payload(),
            &[6]
        );
    }
    assert!(crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before));
}
#[tokio::test]
async fn changed_actual_qualification_generation_retires_old_armed_proposal() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "changed_actual_qualification_generation_retires_old_armed_proposal",
    ) {
        return;
    }
    let state = qualified().await;
    let baseline_state = state.local_flight_state_snapshot();
    let baseline_sdk = cached_identity(&state);
    let observation = Observation::begin(&state).unwrap();
    let Some(original) = observation.captured() else {
        assert_eq!(observation.captured().is_none(), crate::runtime_mode::is_hitl());
        return;
    };
    let generation = state
        .gse
        .lock()
        .unwrap()
        .operational
        .saved
        .qualification
        .generation();
    let proposal = original.proposal(
        FlightState::Armed,
        Cause::QualifiedArmed {
            qualification_generation: generation,
        },
    );
    if crate::runtime_mode::is_test_fire() {
        assert!(proposal.is_none());
        return;
    }
    {
        let mut rt = state.gse.lock().unwrap();
        rt.operational.saved.qualification.reset().unwrap();
        rt.operational.persist().unwrap();
    }
    let before = crate::network_variables::automatic_idle_test_snapshot();
    assert!(!crate::automatic_flight_state::publish(
        &state,
        proposal.unwrap()
    ));
    assert_eq!(state.local_flight_state_snapshot(), baseline_state);
    assert_eq!(cached_identity(&state), baseline_sdk);
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
}

#[tokio::test]
async fn known_actual_board_abort_vetoes_otherwise_qualified_claimed_armed_owner() {
    crate::mode_suite!(Normal, TestFire, Testing);
    if crate::operator_queue::isolated_case(module_path!(),"known_actual_board_abort_vetoes_otherwise_qualified_claimed_armed_owner") {return;}
    let state=qualified().await;
    let baseline_state = state.local_flight_state_snapshot();
    let baseline_sdk = cached_identity(&state);
    let router=state.topology_router.get().unwrap();
    let packet=sedsnet::packet::Packet::new(crate::telemetry_schema::data_type("FLIGHT_STATE"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],Board::ActuatorBoard.sender_id(),
        router.network_time_ms().unwrap(),Arc::<[u8]>::from([15])).unwrap();
    // Actual production raw-ingress classification; no private row/counter
    // mutation. This unit control is not a physical radio/Launch qualification.
    crate::board_abort::observe_raw(&state,&packet);
    assert_ne!(state.board_abort.masks().0,0);
    let before=crate::network_variables::automatic_idle_test_snapshot();
    let observation=Observation::begin(&state).unwrap();
    assert!(observation.captured().is_none());
    publish_qualified_flight_armed_with_observation(&state,&observation);
    assert_eq!(state.local_flight_state_snapshot(),baseline_state);
    assert_eq!(cached_identity(&state), baseline_sdk);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(),before);
    assert_ne!(state.board_abort.masks().0,0);
}
