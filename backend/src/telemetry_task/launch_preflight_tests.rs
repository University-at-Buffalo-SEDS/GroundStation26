// Production physical handlers + actual SDK typed peer transport. No outputs.
async fn fixture() -> (Arc<AppState>, Arc<Router>) {
    let state = crate::state::tests::test_app_state().await;
    state.set_local_flight_state(FlightState::Idle);
    state.toggle_hitl_safety_interlock(3); // This suite specifically exercises enabled ignition-proof preflight.
    let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
        crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
    ]).with_sender("GS")));
    router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
    state.topology_router.set(router.clone()).unwrap();
    (state, router)
}

#[tokio::test]
async fn missing_pair_preflight_keeps_both_physical_launch_handlers_unretained() {
    if crate::operator_queue::isolated_native_case(module_path!(), "missing_pair_preflight_keeps_both_physical_launch_handlers_unretained") { return; }
    for local in [false, true] {
        let (state, router) = fixture().await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        if local {
            handle_local_ground_station_launch_command(state.clone(), router.clone(), revision).await;
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
        }
        assert!(!state.launch_history.lock().unwrap().automatic_actions_inhibited());
        assert!(!state.launch_indicator_latched());
        for name in ["FLIGHT_COMMAND", "VALVE_COMMAND"] {
            assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(name)));
        }
        state.request_shutdown();
    }
}

#[tokio::test]
async fn actual_pair_preflight_admits_original_launch_once_and_keeps_retained_history() {
    if crate::operator_queue::isolated_native_case(module_path!(), "actual_pair_preflight_admits_original_launch_once_and_keeps_retained_history") { return; }
    let (state, router) = fixture().await;
    let _peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
    assert!(crate::ignition_pair::readiness(&state).is_ok());
    let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
    handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
    assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
    assert!(state.launch_indicator_latched(), "original intent must not reject its own retained inhibition");
    assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("FLIGHT_COMMAND")));
    let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
    let second = crate::gse::inhibit_automatic_actions_for_launch_revision(&state, revision);
    // HITL re-sends are allowed: the firmware ignores a Launch while its sequence
    // is active, and retained history stays set for automatic actions.
    assert_eq!(second.is_ok(), cfg!(feature = "hitl_mode"),
        "only HITL may send a second intent over retained Launch history");
    assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
    state.request_shutdown();
}

#[tokio::test]
async fn missing_actual_route_preflight_keeps_journal_unretained_with_fresh_pair() {
    if crate::operator_queue::isolated_native_case(module_path!(), "missing_actual_route_preflight_keeps_journal_unretained_with_fresh_pair") { return; }
    let (state, router) = fixture().await;
    let _peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
    assert!(crate::ignition_pair::readiness(&state).is_ok());
    let sides = router.export_topology().routes.iter().map(|route| route.side_id).collect::<std::collections::BTreeSet<_>>();
    for side in sides {
        router.remove_side(side).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while crate::ignition_pair::require_routes(&state).is_ok() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.unwrap();
    assert!(state.board_abort.ignition_pair_current(), "isolate route failure from stale board state");
    let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
    handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
    assert!(!state.launch_history.lock().unwrap().automatic_actions_inhibited());
    assert!(!state.launch_indicator_latched());
    state.request_shutdown();
}
