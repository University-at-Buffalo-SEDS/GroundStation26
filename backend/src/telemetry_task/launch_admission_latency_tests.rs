// Actual SDK admission after a bounded main-thread preparation stall. No
// physical side is opened and no device command is executed by this fixture.
#[tokio::test]
async fn slow_launch_preparation_keeps_healthy_unchanged_routes_admissible() {
    crate::mode_suite!(Hitl);
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

// One actual SDK UTC authority per isolated process. Both Launch variants use
// this same controllable tick source; OnceLock is never rebound to a new Router.
struct LaunchStatusClock {
    ticks: Arc<std::sync::atomic::AtomicU64>,
    authority: Arc<Router>,
}
impl LaunchStatusClock {
    fn new() -> Self {
        let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let source = ticks.clone();
        let authority = Arc::new(Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
            Box::new(move || source.load(std::sync::atomic::Ordering::Acquire)),
        ));
        authority
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                1_793_563_200_000,
            ))
            .unwrap();
        set_network_time_router(authority.clone());
        let clock = Self { ticks, authority };
        assert_eq!(recording_network_utc_ms(), Some(clock.utc() as i64));
        clock
    }
    fn utc(&self) -> u64 {
        self.authority.network_time_ms().unwrap()
    }
    fn advance(&self, delta: u64) {
        self.ticks
            .fetch_add(delta, std::sync::atomic::Ordering::AcqRel);
        assert_eq!(recording_network_utc_ms(), Some(self.utc() as i64));
    }
    fn router(&self, state: &Arc<AppState>) -> Arc<Router> {
        let source = self.ticks.clone();
        let router = Arc::new(Router::new_with_clock(
            sedsnet::router::RouterConfig::new([crate::ignition_pair::fixture::handler(
                state,
                |_| Ok(()),
            )])
            .with_sender("GS"),
            Box::new(move || source.load(std::sync::atomic::Ordering::Acquire)),
        ));
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                self.utc(),
            ))
            .unwrap();
        state.topology_router.set(router.clone()).unwrap();
        router
    }
    fn status(&self, timestamp: u64) -> Packet {
        assert_eq!(recording_network_utc_ms(), Some(self.utc() as i64));
        Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "VB",
            timestamp,
            Arc::from([ValveBoardCommands::Sequence as u8, 1]),
        )
        .unwrap()
    }
}

#[tokio::test]
async fn accepted_hitl_launch_after_clock_reset_waits_for_valve_status_then_starts_new_cycle() {
    crate::mode_suite!(Hitl);
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "accepted_hitl_launch_after_clock_reset_waits_for_valve_status_then_starts_new_cycle",
    ) {
        return;
    }
    let clock = LaunchStatusClock::new();
    for local in [false, true] {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        for index in 0..4 {
            if state.hitl_safety_interlock_enabled(index) {
                state.toggle_hitl_safety_interlock(index);
            }
        }
        let router = clock.router(&state);
        state.reset_launch_clock();
        let _peers =
            crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(
            &state,
            if local {
                "GroundStationLaunch"
            } else {
                "Launch"
            },
        );
        if local {
            handle_local_ground_station_launch_command(state.clone(), router.clone(), revision)
                .await;
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
        }
        assert!(
            state.launch_indicator_latched(),
            "local={local}: genuine SDK admission must succeed"
        );
        assert!(
            state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        let (daq_pair, daq_revision) = state.launch_history.lock().unwrap().daq_snapshot();
        assert!(
            daq_pair[0] != 0 && daq_pair[1] != 0 && daq_revision != 0,
            "local={local}: accepted Launch publishes GS DAQ request, not a DAQ receipt"
        );
        assert_eq!(
            state.launch_clock_snapshot().kind,
            LaunchClockKind::Idle,
            "SDK acceptance is not valve sequence-start proof"
        );
        assert_eq!(state.operator_queue.fill_launch_activity(), None);
        for source in [clock.utc() - 2_001, clock.utc() + 101] {
            let invalid = clock.status(source);
            apply_umbilical_status_at_ingress(&state, &invalid);
            assert_eq!(
                state.operator_queue.fill_launch_activity(),
                None,
                "stale/future source must not acquire the real sequence activity lease"
            );
            assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle);
        }
        let status = clock.status(clock.utc());
        apply_umbilical_status_at_ingress(&state, &status);
        assert_eq!(
            state.operator_queue.fill_launch_activity(),
            Some(true),
            "local={local}: real fresh VB capture must pass active-lease validation"
        );
        let started = state.launch_clock_snapshot();
        assert_eq!(started.kind, LaunchClockKind::TMinus, "local={local}");
        clock.advance(25);
        apply_umbilical_status_at_ingress(&state, &status);
        assert_eq!(
            state.launch_clock_snapshot(),
            started,
            "same original packet cannot reanchor the accepted countdown"
        );
        let renamed = clock.status(status.timestamp());
        assert_ne!(renamed.packet_id(), status.packet_id());
        apply_umbilical_status_at_ingress(&state, &renamed);
        assert_eq!(
            state.launch_clock_snapshot(),
            started,
            "a new packet ID for the same source capture cannot reanchor countdown"
        );
        state.request_shutdown();
    }
}

#[tokio::test]
async fn stopped_launch_cannot_rearm_reset_clock_from_late_valve_status() {
    crate::mode_suite!(Hitl);
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "stopped_launch_cannot_rearm_reset_clock_from_late_valve_status",
    ) {
        return;
    }
    let clock = LaunchStatusClock::new();
    for local in [false, true] {
        let state = crate::state::tests::test_app_state().await;
        state.set_local_flight_state(FlightState::Idle);
        let router = clock.router(&state);
        state.reset_launch_clock();
        let _peers =
            crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        let revision = crate::board_abort::capture_launch_revision_named(&state, "Launch");
        state.manual_abort_intent.request_untimed();
        if local {
            handle_local_ground_station_launch_command(state.clone(), router.clone(), revision)
                .await;
        } else {
            handle_flight_computer_launch_command(state.clone(), router.clone(), revision).await;
        }
        assert!(!state.launch_indicator_latched());
        assert!(
            !state
                .launch_history
                .lock()
                .unwrap()
                .automatic_actions_inhibited()
        );
        let status = clock.status(clock.utc());
        apply_umbilical_status_at_ingress(&state, &status);
        assert_eq!(
            state.operator_queue.fill_launch_activity(),
            Some(true),
            "fresh owned VB status must be accepted; Stop refusal must not false-pass through stale rejection"
        );
        assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle);
        assert_eq!(
            state.launch_history.lock().unwrap().daq_snapshot().0,
            [0, 0]
        );
        assert!(
            !router
                .has_pending_reliable_tx_type(crate::telemetry_schema::data_type("VALVE_COMMAND"))
        );
        state.request_shutdown();
        clock.advance(1);
    }
}

#[tokio::test]
async fn fresh_valve_sequence_without_accepted_launch_cannot_rearm_reset_clock() {
    crate::mode_suite!(Hitl);
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "fresh_valve_sequence_without_accepted_launch_cannot_rearm_reset_clock",
    ) {
        return;
    }
    let clock = LaunchStatusClock::new();
    let state = crate::state::tests::test_app_state().await;
    state.set_local_flight_state(FlightState::Idle);
    let _router = clock.router(&state);
    state.reset_launch_clock();
    let status = clock.status(clock.utc());
    apply_umbilical_status_at_ingress(&state, &status);
    assert_eq!(
        state.operator_queue.fill_launch_activity(),
        Some(true),
        "fresh VB observation passes; absent accepted intent independently prevents reset-cycle rearm"
    );
    assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle);
    assert!(!state.launch_indicator_latched());
    assert!(
        !state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    assert_eq!(
        state.launch_history.lock().unwrap().daq_snapshot().0,
        [0, 0]
    );
    state.request_shutdown();
}
