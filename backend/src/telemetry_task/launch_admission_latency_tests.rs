// Actual SDK admission after a bounded main-thread preparation stall. No
// physical side is opened and no device command is executed by this fixture.
#[tokio::test]
async fn slow_launch_preparation_keeps_healthy_unchanged_routes_admissible() {
    if crate::operator_queue::isolated_native_case(module_path!(), "slow_launch_preparation_keeps_healthy_unchanged_routes_admissible") { return; }
    let mut failures = Vec::new();
    for local in [false, true] {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        for index in 0..4 {
            if state.hitl_safety_interlock_enabled(index) {
                state.toggle_hitl_safety_interlock(index);
            }
        }
        state.toggle_hitl_safety_interlock(3); // Explicitly enforce the route/proof lease in this stall regression.
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
        ]).with_sender("GS")));
        router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
        state.topology_router.set(router.clone()).unwrap();
        let _peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        assert!(crate::ignition_pair::readiness(&state).is_ok());
        let generation = crate::gse::automatic_generation(&state);
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        let ticket = crate::gse::inhibit_automatic_actions_for_launch_revision(&state, revision).unwrap();
        assert_eq!(crate::gse::automatic_generation(&state), generation,
            "saving the launch intent does not advance the reset generation");
        let mut packets = vec![build_launch_packet(&router,"VALVE_COMMAND","VALVE_BOARD", &[ValveBoardCommands::Sequence as u8]).unwrap()];
        if !local {
            packets.insert(0,build_launch_packet(&router,"FLIGHT_COMMAND","FLIGHT_CONTROLLER", &[FlightComputerCommands::Launch as u8]).unwrap());
        }
        // Model a slow durable write or scheduler stall on the owning async
        // thread. Route topology, board boot, epochs and transport identities
        // are unchanged; no fresh observer job is fabricated during this wait.
        std::thread::sleep(Duration::from_millis(250));
        assert!(state.board_abort.ignition_pair_current());
        let admission = crate::gse::queue_admitted_launch_packets(&state,&router,ticket,packets);
        eprintln!("POST_PREFLIGHT_STALL local={local} admitted={admission:?} retained={} generation_unchanged={} valve_pending={}",
            state.launch_history.lock().unwrap().automatic_actions_inhibited(),
            crate::gse::automatic_generation(&state)==generation,
            router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
        match admission {
            Ok(count) => {
                assert_eq!(count, if local {1} else {2});
                assert!(router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
            }
            Err(error) => failures.push(format!("local={local}: {error}")),
        }
        state.request_shutdown();
    }
    assert!(failures.is_empty(), "a bounded preparation stall must not reject unchanged connected routes after retaining launch intent: {failures:?}");
}

#[tokio::test]
async fn accepted_hitl_launch_after_clock_reset_waits_for_valve_status_then_starts_new_cycle() {
    if crate::operator_queue::isolated_native_case(module_path!(), "accepted_hitl_launch_after_clock_reset_waits_for_valve_status_then_starts_new_cycle") { return; }
    let mut failures = Vec::new();
    for local in [false, true] {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        for index in 0..4 {
            if state.hitl_safety_interlock_enabled(index) {
                state.toggle_hitl_safety_interlock(index);
            }
        }
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
        ]).with_sender("GS")));
        router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
        state.topology_router.set(router.clone()).unwrap();
        state.reset_launch_clock();
        let _peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, if local { "GroundStationLaunch" } else { "Launch" });
        if local {
            handle_local_ground_station_launch_command(state.clone(), router.clone(), revision).await;
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
        }
        assert!(state.launch_indicator_latched(), "local={local}: genuine SDK admission must succeed for this clock regression");
        assert!(state.launch_history.lock().unwrap().automatic_actions_inhibited());
        let (daq_pair, daq_revision) = state.launch_history.lock().unwrap().daq_snapshot();
        if daq_pair[0] == 0 || daq_pair[1] == 0 || daq_revision == 0 {
            failures.push(format!("local={local}: accepted launch did not publish a DAQ session/deadline"));
        }
        // This is a persisted GS request, not a DAQ receipt or SD-writing ACK.
        assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle,
            "SDK acceptance is not valve sequence-start proof");
        let status = Packet::new(crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "VB", router.network_time_ms().unwrap(),
            Arc::from([ValveBoardCommands::Sequence as u8, 1])).unwrap();
        apply_umbilical_status_at_ingress(&state, &status);
        let actual = state.launch_clock_snapshot();
        eprintln!("ACCEPTED_LAUNCH_RESET_CLOCK local={local} after_actual_VB_sequence_status={actual:?}");
        if actual.kind != LaunchClockKind::TMinus {
            failures.push(format!("local={local}: actual valve start left clock {:?}", actual.kind));
        }
        state.request_shutdown();
    }
    assert!(failures.is_empty(), "accepted launch must prepare the new clock cycle before actual valve status: {failures:?}");
}

#[tokio::test]
async fn stopped_launch_cannot_rearm_reset_clock_from_late_valve_status() {
    if crate::operator_queue::isolated_native_case(module_path!(), "stopped_launch_cannot_rearm_reset_clock_from_late_valve_status") { return; }
    for local in [false, true] {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        let router = Arc::new(Router::new(sedsnet::router::RouterConfig::new([
            crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
        ]).with_sender("GS")));
        router.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_793_563_200_000)).unwrap();
        state.topology_router.set(router.clone()).unwrap();
        state.reset_launch_clock();
        let _peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        state.manual_abort_intent.request_untimed();
        if local {
            handle_local_ground_station_launch_command(state.clone(), router.clone(), revision).await;
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
        }
        assert!(!state.launch_indicator_latched());
        assert!(!state.launch_history.lock().unwrap().automatic_actions_inhibited());
        let status = Packet::new(crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "VB", router.network_time_ms().unwrap(),
            Arc::from([ValveBoardCommands::Sequence as u8, 1])).unwrap();
        apply_umbilical_status_at_ingress(&state, &status);
        assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle);
        assert_eq!(state.launch_history.lock().unwrap().daq_snapshot().0, [0, 0]);
        assert!(!router.has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND")));
        state.request_shutdown();
    }
}
