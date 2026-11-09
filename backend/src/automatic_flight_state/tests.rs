// Actual public managed-SDK admission and durable-file tests. No MCU/hardware.
use super::*;
use sedsnet::router::{Router, RouterConfig};

pub(crate) fn isolated(name: &str) -> bool {
    crate::operator_queue::isolated_case(module_path!(), name)
}
pub(crate) async fn ready() -> (Arc<AppState>, Arc<Router>) {
    // The actual GS bootstrap configures its public SDK ring with this helper.
    // SDK RouterConfig defaults reserve only 128 bytes, not the GS 4096 bytes.
    ready_with_config(RouterConfig::new([]).with_sender("GS")
        .with_memory_config(crate::network_memory_config()).unwrap()).await
}
async fn ready_with_config(config: RouterConfig) -> (Arc<AppState>, Arc<Router>) {
    crate::telemetry_schema::initialize().unwrap();
    let _ = crate::network_variables::flight_state();
    let state = crate::state::tests::test_app_state().await;
    let root = std::path::PathBuf::from(std::env::var_os("GS_RECOVERY_ISOLATED_ROOT").unwrap());
    *state.launch_history.lock().unwrap() =
        crate::launch_history::Store::open(root.join("owned-clock.json"), false, None, 10000, 0)
            .unwrap();
    let router = Arc::new(Router::new(config));
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    crate::network_variables::initialize_for_state(&state, &router).unwrap();
    state.topology_router.set(router.clone()).unwrap();
    (state, router)
}
fn cached(router: &Router) -> Option<sedsnet::packet::Packet> {
    router
        .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
        .unwrap()
}
// Healthy multi-phase fixtures service the same public SDK queues as the host.
// This generic observer is an actual Router, not modeled MCU proof or hardware.
struct PhasePeer {
    router: Router,
    host_side: usize,
    peer_side: usize,
    outgoing: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    incoming: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    received: Arc<std::sync::Mutex<Vec<sedsnet::packet::Packet>>>,
}
impl PhasePeer {
    async fn connect(host: &Router) -> Self {
        let endpoint = crate::telemetry_schema::endpoint("FLIGHT_STATE");
        let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
        let peer = Router::new(RouterConfig::new([
            sedsnet::router::EndpointHandler::new_packet_handler(endpoint, |_| Ok(())),
        ]).with_sender("PHASE_OBSERVER"));
        peer.enable_network_variable(ty, sedsnet::router::NetworkVariablePermissions::READ_ONLY).unwrap();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = received.clone();
        peer.on_network_variable_update(ty, move |packet| {
            capture.lock().unwrap().push(packet.clone());
            Ok(())
        }).unwrap();
        let outgoing = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = outgoing.clone();
        let host_side = host.add_side_packed("phase-peer", move |wire| {
            output.lock().unwrap().push(wire.to_vec());
            Ok(())
        });
        let incoming = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = incoming.clone();
        let peer_side = peer.add_side_packed("phase-host", move |wire| {
            output.lock().unwrap().push(wire.to_vec());
            Ok(())
        });
        let result = Self { router: peer, host_side, peer_side, outgoing, incoming, received };
        host.announce_discovery().unwrap();
        result.router.announce_discovery().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                result.service(host);
                if host.export_topology().routes.iter().any(|route|
                    route.side_id == result.host_side && route.reachable_endpoints.contains(&endpoint)) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.expect("actual peer Discovery prerequisite");
        result
    }
    fn service(&self, host: &Router) {
        host.process_all_queues_with_timeout(0).unwrap();
        for wire in std::mem::take(&mut *self.outgoing.lock().unwrap()) {
            self.router.rx_packed_from_side(&wire, self.peer_side).unwrap();
        }
        self.router.process_all_queues_with_timeout(0).unwrap();
        for wire in std::mem::take(&mut *self.incoming.lock().unwrap()) {
            host.rx_packed_from_side(&wire, self.host_side).unwrap();
        }
    }
    async fn accept(&self, host: &Router, original: &sedsnet::packet::Packet) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                self.service(host);
                if self.received.lock().unwrap().iter().any(|packet| {
                    packet.packet_id() == original.packet_id()
                        && packet.timestamp() == original.timestamp()
                        && packet.payload() == original.payload()
                }) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.expect("original phase reaches actual public managed peer");
        // Also process the real acknowledgment/control responses already due;
        // no internal queue clear or synthetic ACK substitutes for retirement.
        self.service(host);
    }
}

fn legacy(observation: &Observation, target: FlightState) -> Proposal {
    let original = observation
        .captured()
        .expect("fresh original normal fixture");
    original
        .proposal(
            target,
            Cause::LegacySequence {
                next_policy: original.policy,
            },
        )
        .unwrap()
}
#[tokio::test]
async fn complete_live_phase_chain_preserves_canonical_json_and_original_ids() {
    crate::mode_suite!(Normal, Testing);
    if isolated("complete_live_phase_chain_preserves_canonical_json_and_original_ids") {
        return;
    }
    let (state, router) = ready().await;
    let peer = PhasePeer::connect(&router).await;
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let mut ids = std::collections::HashSet::new();
    for phase in [
        FlightState::Idle,
        FlightState::PreFill,
        FlightState::NitrogenFill,
        FlightState::FillTest,
        FlightState::NitrousFill,
        FlightState::Armed,
    ] {
        let observation = Observation::begin(&state).unwrap();
        assert!(publish(&state, legacy(&observation, phase)));
        assert_eq!(state.local_flight_state_snapshot(), phase);
        let packet = cached(&router).unwrap();
        peer.accept(&router, &packet).await;
        assert_eq!(packet.payload(), &[phase as u8]);
        assert!(
            ids.insert(packet.packet_id()),
            "each accepted original has its own immutable identity"
        );
        assert!(
            crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before)
        );
        assert_eq!(crate::network_variables::flight_state(), 0);
    }
    crate::network_variables::set_flight_buzzer(&router, true).unwrap();
    assert_eq!(
        crate::network_variables::flight_state(),
        0,
        "unrelated setting must not promote liveArmed"
    );
    let path = std::path::PathBuf::from(std::env::var_os("GS_NETWORK_VARIABLE_CACHE").unwrap());
    let bytes = std::fs::read(path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["flight_state"], 0);
    let restarted = Router::new(RouterConfig::new([]).with_sender("GS"));
    crate::network_variables::initialize_for_state(&state, &restarted).unwrap();
    assert!(
        cached(&restarted).is_none(),
        "live identity must suppress conflicting bootstrap seed"
    );
}
#[tokio::test]
async fn retained_stop_and_stop_after_capture_leave_live_sdk_and_json_unchanged() {
    crate::mode_suite!(Normal, Testing);
    if isolated("retained_stop_and_stop_after_capture_leave_live_sdk_and_json_unchanged") {
        return;
    }
    let (state, router) = ready().await;
    let observation = Observation::begin(&state).unwrap();
    let proposal = legacy(&observation, FlightState::Idle);
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let old = cached(&router);
    let epoch = state.manual_abort_intent.request();
    assert!(!publish(&state, proposal));
    assert!(Observation::begin(&state).unwrap().captured().is_none());
    assert_eq!(state.manual_abort_intent.pending_epoch(), Some(epoch));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(
        cached(&router).map(|p| p.packet_id()),
        old.map(|p| p.packet_id())
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
}
#[tokio::test]
async fn explicit_same_phase_store_revision_retires_old_automatic_original() {
    crate::mode_suite!(Normal, Testing);
    if isolated("explicit_same_phase_store_revision_retires_old_automatic_original") {
        return;
    }
    let (state, router) = ready().await;
    let observation = Observation::begin(&state).unwrap();
    let proposal = legacy(&observation, FlightState::Idle);
    crate::network_variables::set_flight_state(&router, FlightState::Startup as u8).unwrap();
    let explicit = cached(&router).unwrap();
    let after_explicit = crate::network_variables::automatic_idle_test_snapshot();
    assert!(!publish(&state, proposal));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(cached(&router).unwrap().packet_id(), explicit.packet_id());
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        after_explicit
    );
}
#[tokio::test]
async fn actual_closed_barrier_refuses_old_original_without_epoch_assumptions() {
    crate::mode_suite!(Normal, Testing);
    if isolated("actual_closed_barrier_refuses_old_original_without_epoch_assumptions") {
        return;
    }
    let (state, router) = ready().await;
    let observation = Observation::begin(&state).unwrap();
    let proposal = legacy(&observation, FlightState::Idle);
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let generation = observation.generation();
    crate::gse::reset_generation::close_operator_admission(&state, generation, 781).unwrap();
    assert!(!publish(&state, proposal));
    assert!(
        crate::gse::reset_generation::invalidate_capture_epoch(&state, generation).is_err(),
        "actual original lease still blocks verified epoch advance"
    );
    assert_eq!(crate::gse::automatic_generation(&state), generation);
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert!(cached(&router).is_none());
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
}
#[tokio::test]
async fn history_launch_inhibit_blocks_auto_even_before_phase_or_lamp_catches_up() {
    crate::mode_suite!(Normal, Testing);
    if isolated("history_launch_inhibit_blocks_auto_even_before_phase_or_lamp_catches_up") {
        return;
    }
    let (state, router) = ready().await;
    let observation = Observation::begin(&state).unwrap();
    let proposal = legacy(&observation, FlightState::Idle);
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .unwrap();
    assert!(!state.launch_indicator_latched());
    assert!(!publish(&state, proposal));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert!(cached(&router).is_none());
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
}
#[tokio::test]
async fn operator_mode_startup_cannot_borrow_normal_readiness_authority() {
    crate::mode_suite!(Hitl, TestFire);
    if isolated("operator_mode_startup_cannot_borrow_normal_readiness_authority") {
        return;
    }
    let (state, router) = ready().await;
    let observation = Observation::begin(&state).unwrap();
    if let Some(original) = observation.captured() {
        assert!(
            original
                .proposal(
                    FlightState::Idle,
                    Cause::LegacySequence {
                        next_policy: original.policy
                    }
                )
                .is_none()
        );
    }
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert!(cached(&router).is_none());
}
#[tokio::test]
async fn every_valid_clock_kind_survives_all_live_prelaunch_phases_byte_exact() {
    crate::mode_suite!(Normal, Testing);
    if isolated("every_valid_clock_kind_survives_all_live_prelaunch_phases_byte_exact") {
        return;
    }
    let (state, router) = ready().await;
    let peer = PhasePeer::connect(&router).await;
    use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
    for clock in [
        LaunchClockMsg::idle(),
        LaunchClockMsg {
            kind: LaunchClockKind::TMinus,
            anchor_timestamp_ms: Some(1_793_000_000_000),
            duration_ms: Some(15000),
        },
        LaunchClockMsg {
            kind: LaunchClockKind::TPlus,
            anchor_timestamp_ms: Some(1_793_000_000_000),
            duration_ms: None,
        },
    ] {
        state.set_local_flight_state(FlightState::Startup); // explicit fixture new pass
        *state.launch_clock.lock().unwrap() = clock.clone(); // valid input fixture only
        let before = crate::network_variables::automatic_idle_test_snapshot();
        for phase in [
            FlightState::Idle,
            FlightState::PreFill,
            FlightState::NitrogenFill,
            FlightState::FillTest,
            FlightState::NitrousFill,
            FlightState::Armed,
        ] {
            let observation = Observation::begin(&state).unwrap();
            assert!(publish(&state, legacy(&observation, phase)));
            peer.accept(&router, &cached(&router).unwrap()).await;
            assert_eq!(state.launch_clock_snapshot(), clock);
            assert!(
                crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before)
            );
        }
    }
}

#[tokio::test]
async fn actual_sdk_default_ring_capacity_refuses_next_phase_without_partial_acceptance() {
    crate::mode_suite!(Normal, Testing);
    if isolated("actual_sdk_default_ring_capacity_refuses_next_phase_without_partial_acceptance") {
        return;
    }
    assert_eq!(sedsnet::config::RuntimeMemoryConfig::default().starting_queue_size, 128);
    let (state, router) = ready_with_config(RouterConfig::new([]).with_sender("GS")).await;
    let first = Observation::begin(&state).unwrap();
    assert!(publish(&state, legacy(&first, FlightState::Idle)));
    drop(first);
    let accepted = cached(&router).unwrap();
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let observation = Observation::begin(&state).unwrap();
    let proposal = legacy(&observation, FlightState::PreFill);
    let packet = crate::network_variables::automatic_phase_packet(&router, proposal.target as u8).unwrap();
    let original_id = packet.packet_id();
    let original_utc = packet.timestamp();
    let identity = crate::network_variables::AutomaticPhaseIdentity {
        original: crate::network_variables::AutomaticIdleIdentity {
            packet_id: original_id, timestamp: original_utc, generation: observation.generation(),
        }, phase: proposal.target as u8,
    };
    let mut prepared = router.prepare_network_variable_batch(vec![packet],
        crate::network_variables::automatic_idle_batch_limits()).unwrap_or_else(|failure| panic!("actual preparation: {:?}", failure.error));
    let model = crate::flight_sim::prepare_automatic_phase(proposal.original.expected, proposal.target);
    assert!(matches!(crate::gse::try_commit_automatic_phase(&state, proposal, identity, &model, &mut prepared),
        Err(Error::Sdk(sedsnet::router::TryManagedBatchError::Capacity))));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    assert_eq!(crate::network_variables::automatic_idle_test_snapshot(), before);
    let unchanged = cached(&router).unwrap();
    assert_eq!((unchanged.packet_id(), unchanged.timestamp(), unchanged.payload()),
        (accepted.packet_id(), accepted.timestamp(), accepted.payload()));
    let returned = prepared.into_packets().expect("Capacity leaves exact originals caller owned");
    assert_eq!(returned.len(), 1);
    assert_eq!((returned[0].packet_id(), returned[0].timestamp(), returned[0].payload()),
        (original_id, original_utc, &[FlightState::PreFill as u8][..]));
}
