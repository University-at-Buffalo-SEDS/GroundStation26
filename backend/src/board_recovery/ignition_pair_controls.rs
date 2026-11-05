// Protocol-positive peers exist before the wire starts. Proof is established
// only by actual HTTP QUERY -> SDK peer -> correlated SDK ingress, never masks.
async fn ignition_valid_fixture() -> Fixture {
    let owners = BOARDS
        .into_iter()
        .map(|board| {
            (
                board,
                Arc::new(Mutex::new(Model {
                    hardened: true,
                    manual_v1: false,
                    boot: 42,
                    epoch: 8,
                    state: 1,
                    result: 0,
                    applied: 0,
                    requests: Vec::new(),
                    seen: HashMap::new(),
                    pending_only: false,
                    fail_result: None,
                    output_override: None,
                    fail_first_readback: false,
                    retained_first: None,
                })),
            )
        })
        .collect();
    Fixture::from_saved_with_services_profile(None, Some(owners), None, true, false, true).await
}
async fn ignition_query_pair(fixture: &Fixture) {
    fixture.query(Board::ActuatorBoard).await;
    fixture.query(Board::ValveBoard).await;
}
async fn ignition_hold_transport(fixture: &Fixture) {
    fixture.ignition_wire_hold.store(1, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.ignition_wire_hold.load(Ordering::Acquire) != 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("actual fixture wire must acknowledge transport hold");
    assert!(!fixture.pump.is_finished());
}
// Direct SDK mutations have no application transport event. Await the bounded
// observer's actual post-mutation snapshot rather than claiming atomic routes.
async fn ignition_observe_after_sdk_change(state: &AppState, ready: bool) {
    let after = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if crate::ignition_pair::observer::observed_after(state, after, ready) { break; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("a newly captured actual SDK route/side snapshot must show the change");
}
fn ignition_ab_side(state: &AppState) -> usize {
    state
        .topology_router
        .get()
        .unwrap()
        .export_topology()
        .routes
        .into_iter()
        .find(|side| {
            side.reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint("ACTUATOR_BOARD"))
        })
        .expect("actual discovered AB route")
        .side_id
}
fn ignition_try_raw(fixture: &Fixture, raw: u8) -> sedsnet::TelemetryResult<bool> {
    let state = fixture.state();
    crate::gse::queue_controller_with_stop_fence(
        &state,
        state.topology_router.get().unwrap(),
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        &[raw],
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_actual_queries_admit_both_raw14_variants() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_actual_queries_admit_both_raw14_variants") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    assert!(crate::ignition_pair::readiness(&fixture.state()).is_ok());
    for raw in [14, 14 | 0x80] {
        assert!(ignition_try_raw(&fixture, raw).unwrap());
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_one_fresh_owner_does_not_admit_sequence() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_one_fresh_owner_does_not_admit_sequence") { return; }
    let fixture = ignition_valid_fixture().await;
    fixture.query(Board::ActuatorBoard).await;
    assert!(!fixture.state().board_abort.ignition_pair_current());
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_stale_owner_with_fresh_other_owner_refuses() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_stale_owner_with_fresh_other_owner_refuses") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    tokio::time::sleep(Duration::from_millis(5050)).await;
    fixture.query(Board::ValveBoard).await;
    assert!(!fixture.state().board_abort.ignition_pair_current());
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_actual_abort_readback_refuses_sequence() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_actual_abort_readback_refuses_sequence") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    fixture
        .models
        .iter()
        .find(|(board, _)| *board == Board::ValveBoard)
        .unwrap()
        .1
        .lock()
        .unwrap()
        .state = 15;
    fixture.query(Board::ValveBoard).await;
    assert!(!fixture.state().board_abort.ignition_pair_current());
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_route_loss_after_real_proof_refuses_first_owner() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_route_loss_after_real_proof_refuses_first_owner") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    let state = fixture.state();
    assert!(state.board_abort.ignition_pair_current());
    let host = state.topology_router.get().unwrap();
    ignition_hold_transport(&fixture).await;
    host.remove_side(ignition_ab_side(&state)).unwrap();
    ignition_observe_after_sdk_change(&state, false).await;
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
    assert!(
        !host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ACTUATOR_COMMAND"))
    );
    // Do not yield the fixture wire after intentionally removing its side.
}
#[cfg(feature = "hitl_mode")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_hitl_optional_flags_and_viewer_cannot_bypass() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_hitl_optional_flags_and_viewer_cannot_bypass") { return; }
    let fixture = ignition_valid_fixture().await;
    let state = fixture.state();
    state.hitl_safety_interlocks[3].store(false, std::sync::atomic::Ordering::Release);
    state
        .hitl_button_interlock_enabled
        .store(false, std::sync::atomic::Ordering::Release);
    assert!(!state.is_command_allowed(&crate::types::TelemetryCommand::IgniterSequence));
    for principal in ["viewer", "operator"] {
        use tower::ServiceExt;
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/command")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {principal}"))
            .body(axum::body::Body::from(json!("IgniterSequence").to_string()))
            .unwrap();
        let response = crate::web::router(state.clone(), "fixture-only".into())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    ignition_query_pair(&fixture).await;
    assert!(state.is_command_allowed(&crate::types::TelemetryCommand::IgniterSequence));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_policy_disables_sequence_without_disabling_manual_off_catalog() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_policy_disables_sequence_without_disabling_manual_off_catalog") { return; }
    let fixture = ignition_valid_fixture().await;
    let mut policy = crate::sequences::ActionPolicyMsg {
        key_enabled: true,
        software_buttons_enabled: true,
        controls: [
            "Launch",
            "GroundStationLaunch",
            "IgniterSequence",
            "Igniter",
            "Abort",
        ]
        .into_iter()
        .map(|name| crate::sequences::ActionControl {
            cmd: name.into(),
            enabled: true,
            blink: crate::sequences::BlinkMode::Slow,
            actuated: None,
        })
        .collect(),
    };
    crate::ignition_pair::project_policy(&fixture.state(), &mut policy);
    for row in policy.controls {
        assert_eq!(row.enabled, matches!(row.cmd.as_str(), "Igniter" | "Abort"));
    }
}
fn ignition_launch_bundle(host: &Router) -> Vec<Packet> {
    let source = crate::system_clock::control_packet_timestamp_ms(host);
    [
        (
            "FLIGHT_COMMAND",
            "FLIGHT_CONTROLLER",
            crate::rocket_commands::FlightComputerCommands::Launch as u8,
        ),
        (
            "VALVE_COMMAND",
            "VALVE_BOARD",
            crate::rocket_commands::ValveBoardCommands::Sequence as u8,
        ),
    ]
    .into_iter()
    .map(|(kind, endpoint, value)| {
        Packet::new(
            crate::telemetry_schema::data_type(kind),
            &[
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                crate::telemetry_schema::endpoint(endpoint),
            ],
            &host.sender(),
            source,
            Arc::from([value]),
        )
        .unwrap()
    })
    .collect()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_missing_vb_blocks_the_entire_fc_and_vb_bundle() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_missing_vb_blocks_the_entire_fc_and_vb_bundle") { return; }
    let fixture = ignition_valid_fixture().await;
    let state = fixture.state();
    #[cfg(feature = "hitl_mode")]
    state.hitl_safety_interlocks[3].store(false, std::sync::atomic::Ordering::Release);
    fixture.query(Board::ActuatorBoard).await;
    let host = state.topology_router.get().unwrap();
    let ticket = crate::gse::plumbing_admission_ticket(&state);
    assert!(
        crate::gse::queue_prepared_launch_packets(
            &state,
            host,
            ticket,
            ignition_launch_bundle(host),
            None
        )
        .is_err()
    );
    for name in ["FLIGHT_COMMAND", "VALVE_COMMAND"] {
        assert!(
            !host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)),
            "whole first bundle requires pair before either owner: {name}"
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_accepted_launch_is_not_automatically_stopped_on_later_route_loss() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_accepted_launch_is_not_automatically_stopped_on_later_route_loss") { return; }
    let fixture = ignition_valid_fixture().await;
    for board in BOARDS {
        fixture.query(board).await;
    }
    let state = fixture.state();
    let host = state.topology_router.get().unwrap();
    let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
    let ticket =
        crate::gse::inhibit_automatic_actions_for_launch_revision(&state, revision).unwrap();
    let bundle = ignition_launch_bundle(host);
    let originals = bundle
        .iter()
        .map(|p| (p.packet_id(), p.timestamp()))
        .collect::<Vec<_>>();
    // A newer observation may conservatively retire a pre-offer token while
    // admission waits for existing guards. Retry only that zero-offer result,
    // retaining the original ticket, packet IDs and source UTC throughout.
    let prepared = if cfg!(feature = "hitl_mode") {
        None
    } else {
        Some(crate::network_variables::prepare_launch_state(host).unwrap())
    };
    let admission_deadline = std::time::Instant::now() + Duration::from_secs(1);
    let accepted = loop {
        assert_eq!(state.ignition_observer.launch_sdk_offers.load(Ordering::Acquire), 0);
        match crate::gse::queue_prepared_launch_packets(&state, host, ticket, bundle.clone(), prepared.clone()) {
            Ok(accepted) => break accepted,
            Err(error) if error == "Ignition route observation changed before SDK admission" => {
                assert_eq!(state.ignition_observer.launch_sdk_offers.load(Ordering::Acquire), 0);
                for kind in ["FLIGHT_COMMAND", "VALVE_COMMAND"] {
                    assert!(!host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(kind)));
                }
                assert!(std::time::Instant::now() < admission_deadline, "bounded original admission: {error}");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("unexpected original admission refusal: {error}"),
        }
    };
    assert!(accepted >= 2);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivered = {
                let actual = fixture.controller_receipts.lock().unwrap();
                originals.iter().all(|identity| {
                    actual
                        .iter()
                        .any(|packet| (packet.packet_id(), packet.timestamp()) == *identity)
                })
            };
            if delivered {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("actual SDK callbacks retain both original IDs and UTC");
    let accepted_state = if cfg!(feature = "hitl_mode") {
        crate::types::FlightState::Startup
    } else {
        crate::types::FlightState::Launch
    };
    assert_eq!(state.local_flight_state_snapshot(), accepted_state);
    assert!(!crate::gse::automatic_abort_allowed(&state));
    ignition_hold_transport(&fixture).await;
    host.remove_side(ignition_ab_side(&state)).unwrap();
    ignition_observe_after_sdk_change(&state, false).await;
    assert!(crate::ignition_pair::require_routes(&state).is_err());
    assert!(!crate::gse::automatic_abort_allowed(&state));
    assert_eq!(state.local_flight_state_snapshot(), accepted_state);
    assert!(
        !crate::gse::queue_automatic_abort_for_generation(
            &state,
            host,
            crate::gse::automatic_generation(&state),
            "must not stop accepted Launch on missing pair"
        )
        .unwrap()
    );
    assert!(!host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ABORT")));
    let actual = fixture.controller_receipts.lock().unwrap();
    for identity in &originals {
        assert_eq!(
            actual
                .iter()
                .filter(|packet| (packet.packet_id(), packet.timestamp()) == *identity)
                .count(),
            1,
            "each original Launch packet is delivered once without replay/retimestamp"
        );
    }
    // No yield after removing a fixture-owned side; Drop shuts down its wire.
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_prepared_packet_cannot_outlive_queried_pair_proof() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_prepared_packet_cannot_outlive_queried_pair_proof") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    // Consume part of the original five-second proof before preparing. The
    // admission hook itself intentionally has a bounded five-second wait.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let state = fixture.state();
    let host = state.topology_router.get().unwrap().clone();
    let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&state);
    let worker_state = state.clone();
    let caller = std::thread::spawn(move || {
        crate::gse::queue_controller_with_stop_fence(
            &worker_state,
            &host,
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[14],
        )
    });
    pause.prepared.recv_timeout(Duration::from_secs(3)).unwrap();
    let original = pause.packet.lock().unwrap().clone().unwrap();
    std::thread::sleep(Duration::from_millis(2050));
    pause.proceed.send(()).unwrap();
    assert!(!matches!(caller.join().unwrap(), Ok(true)));
    assert_eq!(
        pause
            .packet
            .lock()
            .unwrap()
            .as_ref()
            .map(|p| (p.packet_id(), p.timestamp())),
        Some((original.packet_id(), original.timestamp()))
    );
    assert!(
        !state
            .topology_router
            .get()
            .unwrap()
            .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("ACTUATOR_COMMAND"))
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_actual_core_busy_then_route_loss_has_zero_sdk_owners() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_actual_core_busy_then_route_loss_has_zero_sdk_owners") { return; }
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    let owner = Arc::new(AtomicUsize::new(0));
    let clock_owner = owner.clone();
    let start = std::time::Instant::now();
    let clock = Box::new(move || {
        crate::gse::nonwaiting_admission_tests::clock_pause(
            clock_owner.load(AtomicOrdering::Acquire),
        );
        start.elapsed().as_millis() as u64
    });
    let owners = BOARDS
        .into_iter()
        .map(|board| {
            (
                board,
                Arc::new(Mutex::new(Model {
                    hardened: true,
                    manual_v1: false,
                    boot: 42,
                    epoch: 8,
                    state: 1,
                    result: 0,
                    applied: 0,
                    requests: Vec::new(),
                    seen: HashMap::new(),
                    pending_only: false,
                    fail_result: None,
                    output_override: None,
                    fail_first_readback: false,
                    retained_first: None,
                })),
            )
        })
        .collect();
    let fixture = Fixture::from_saved_with_services_profile(
        None,
        Some(owners),
        Some(clock),
        true,
        false,
        true,
    )
    .await;
    let state = fixture.state();
    owner.store(Arc::as_ptr(&state) as usize, AtomicOrdering::Release);
    ignition_query_pair(&fixture).await;
    ignition_hold_transport(&fixture).await;
    let host = state.topology_router.get().unwrap().clone();
    let side = ignition_ab_side(&state);
    let kind = crate::telemetry_schema::data_type("ACTUATOR_COMMAND");
    let seed_kind = crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW");
    host.enable_managed_variable(seed_kind).unwrap();
    let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&state);
    pause.hold_retry();
    let route_pause = crate::ignition_pair::route_test_pause::Pause::install(&state);
    let worker_state = state.clone();
    let worker_host = host.clone();
    let caller = std::thread::Builder::new()
        .name("ignition-pair-new-control".into())
        .spawn(move || {
            crate::gse::queue_controller_with_stop_fence(&worker_state, &worker_host, kind, &[14])
        })
        .unwrap();
    pause.prepared.recv_timeout(Duration::from_secs(3)).unwrap();
    pause.proceed.send(()).unwrap();
    route_pause
        .observed
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    let seed = Packet::new(
        seed_kind,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "GS",
        100,
        Arc::from([0]),
    )
    .unwrap();
    let holder_host = host.clone();
    let holder = std::thread::Builder::new()
        .name("production-sdk-core-holder".into())
        .spawn(move || holder_host.seed_managed_variable(seed))
        .unwrap();
    pause
        .core_entered
        .recv_timeout(Duration::from_secs(3))
        .unwrap();
    route_pause.resume.send(()).unwrap();
    pause.busy.recv_timeout(Duration::from_secs(3)).unwrap();
    pause.release_core();
    holder.join().unwrap().unwrap();
    host.remove_side(side).unwrap();
    ignition_observe_after_sdk_change(&state, false).await;
    assert!(crate::ignition_pair::require_routes(&state).is_err());
    pause.resume_retry();
    assert!(!matches!(caller.join().unwrap(), Ok(true)));
    assert!(!host.has_pending_reliable_tx_type(kind));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignition_pair_actual_disabled_side_flags_refuse_and_restored_side_qualifies() {
    if crate::operator_queue::isolated_native_case(module_path!(), "ignition_pair_actual_disabled_side_flags_refuse_and_restored_side_qualifies") { return; }
    let fixture = ignition_valid_fixture().await;
    ignition_query_pair(&fixture).await;
    ignition_hold_transport(&fixture).await;
    let state = fixture.state();
    let host = state.topology_router.get().unwrap();
    let side = ignition_ab_side(&state);
    assert!(crate::ignition_pair::readiness(&state).is_ok());
    host.set_side_egress_enabled(side, false).unwrap();
    ignition_observe_after_sdk_change(&state, false).await;
    assert!(crate::ignition_pair::readiness(&state).is_err());
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
    host.set_side_egress_enabled(side, true).unwrap();
    ignition_observe_after_sdk_change(&state, true).await;
    assert!(crate::ignition_pair::readiness(&state).is_ok());
    host.set_side_ingress_enabled(side, false).unwrap();
    ignition_observe_after_sdk_change(&state, false).await;
    assert!(crate::ignition_pair::readiness(&state).is_err());
    assert!(!matches!(ignition_try_raw(&fixture, 14), Ok(true)));
    host.set_side_ingress_enabled(side, true).unwrap();
    ignition_observe_after_sdk_change(&state, true).await;
    assert!(crate::ignition_pair::readiness(&state).is_ok());
    assert!(ignition_try_raw(&fixture, 14).unwrap());
}
