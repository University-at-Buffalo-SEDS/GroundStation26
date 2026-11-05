//! Native actual-model/SDK controls, not MCU or hardware qualifications.
use super::*;
use crate::{automatic_startup_idle::Captured, network_variables as variables};
use sedsnet::router::{Router, RouterConfig};
use std::time::{Duration, Instant};

async fn ready() -> (Arc<crate::state::AppState>, Arc<Router>) {
    assert!(sim_mode_enabled());
    crate::telemetry_schema::initialize().unwrap();
    let _ = variables::flight_state();
    let state = crate::state::tests::test_app_state().await;
    *state.state.lock().unwrap() = FlightState::Startup;
    let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    variables::initialize_for_state(&state, &router).unwrap();
    state.topology_router.set(router.clone()).unwrap();
    {
        // Model fixture only, before the tested original admission.
        let mut model = sim().lock().unwrap();
        model.flight_state = FlightState::Startup;
        model.automatic_idle_volatile = false;
        model.queued.clear();
    }
    (state, router)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_readiness_periodic_ticks_do_not_mint_fc_idle_or_promote_restart() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "automatic_readiness_periodic_ticks_do_not_mint_fc_idle_or_promote_restart",
    ) {
        return;
    }
    let (state, router) = ready().await;
    let original = Captured::capture(&state, true).unwrap();
    let before = variables::automatic_idle_test_snapshot();
    crate::automatic_startup_idle::publish(&state, original).await;
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Idle);
    assert!(sim().lock().unwrap().automatic_idle_volatile);
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let original = router.get_cached_network_variable(ty).unwrap().unwrap();
    let started = Instant::now();
    let mut ticks = 0;
    while started.elapsed() < Duration::from_millis(FLIGHT_STATE_PERIOD_MS * 2 + 200) {
        // Actual repeated reception of the accepted original is not new intent.
        router.rx(&original).unwrap();
        if let Some(packet) = _next_state_aware_packet().unwrap() {
            assert_ne!(
                packet.data_type(),
                ty,
                "periodic model must not mint FC state from volatile GS readiness"
            );
        }
        assert!(sim().lock().unwrap().automatic_idle_volatile);
        assert!(variables::automatic_idle_test_snapshot().durable_unchanged(&before));
        ticks += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ticks > 10);
    assert_eq!(variables::flight_state(), 0);
    let restarted = Router::new(RouterConfig::new([]).with_sender("GS"));
    variables::initialize_for_state(&state, &restarted).unwrap();
    restarted.rx(&original).unwrap();
    assert_eq!(
        variables::flight_state(),
        0,
        "old original cannot become a restart seed"
    );

    // Existing explicitly-owned manual/model sync remains authoritative. This
    // is a native owner control, not an HTTP authorization/hardware assertion.
    let generation = crate::gse::capture_operator_origin(&state).unwrap();
    let _claim = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
    sync_local_flight_state(FlightState::Idle);
    assert!(!sim().lock().unwrap().automatic_idle_volatile);
    let started = Instant::now();
    let mut actual_fc = None;
    while started.elapsed() < Duration::from_millis(FLIGHT_STATE_PERIOD_MS + 1200) {
        if let Some(packet) = _next_state_aware_packet().unwrap()
            && packet.data_type() == ty
        {
            actual_fc = Some(packet);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let actual_fc = actual_fc.expect("explicit model authority retains periodic FC report");
    assert_eq!(actual_fc.sender(), "FC");
    assert_eq!(actual_fc.payload(), &[1]);
    router.rx(&actual_fc).unwrap();
    assert_eq!(
        variables::flight_state(),
        1,
        "legitimate FC/manual sequence persistence not broadly filtered"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_model_and_retained_stop_refuse_original_without_any_effect() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "held_model_and_retained_stop_refuse_original_without_any_effect",
    ) {
        return;
    }
    let (state, router) = ready().await;
    let original = Captured::capture(&state, true).unwrap();
    let _claim = crate::gse::claim_operator_dispatch(&state, original.generation).unwrap();
    let packet = variables::automatic_idle_packet(&router).unwrap();
    let identity = variables::AutomaticIdleIdentity {
        packet_id: packet.packet_id(),
        timestamp: packet.timestamp(),
        generation: original.generation,
    };
    let mut batch = router
        .prepare_network_variable_batch(vec![packet], variables::automatic_idle_batch_limits())
        .unwrap();
    let model = prepare_automatic_readiness();
    let before = variables::automatic_idle_test_snapshot();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let cache = router.get_cached_network_variable(ty).unwrap();
    let held = sim().lock().unwrap();
    assert!(matches!(
        crate::gse::try_commit_automatic_idle(&state, original, identity, &model, &mut batch),
        Err(crate::automatic_startup_idle::Error::Busy)
    ));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(variables::automatic_idle_test_snapshot(), before);
    assert_eq!(router.get_cached_network_variable(ty).unwrap(), cache);
    let stop = state.manual_abort_intent.request();
    drop(held);
    assert!(matches!(
        crate::gse::try_commit_automatic_idle(&state, original, identity, &model, &mut batch),
        Err(crate::automatic_startup_idle::Error::Retired)
    ));
    assert_eq!(state.manual_abort_intent.pending_epoch(), Some(stop));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(variables::automatic_idle_test_snapshot(), before);
    assert_eq!(router.get_cached_network_variable(ty).unwrap(), cache);
    assert!(!sim().lock().unwrap().automatic_idle_volatile);
    assert!(sim().lock().unwrap().queued.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_unqualified_idle_ticks_are_volatile_but_owned_launch_sequence_is_preserved() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "initial_unqualified_idle_ticks_are_volatile_but_owned_launch_sequence_is_preserved",
    ) {
        return;
    }
    assert!(sim_mode_enabled());
    crate::telemetry_schema::initialize().unwrap();
    let state = crate::state::tests::test_app_state().await;
    let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    variables::initialize_for_state(&state, &router).unwrap();
    state.topology_router.set(router.clone()).unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    assert_eq!(variables::flight_state(), 0);
    let before = variables::automatic_idle_test_snapshot();
    let prepared = prepare_automatic_readiness();
    {
        let model = sim().lock().unwrap();
        assert_eq!(
            model.flight_state,
            FlightState::Idle,
            "constructor phase unchanged"
        );
        assert!(
            model.automatic_idle_volatile,
            "implicit initial TestFire Idle has no FC authority"
        );
    }
    // The real managed request API is exercised; no claim is made that a GET
    // traversed a physical/peer wire in this native no-peer model control.
    router.request_managed_variable(ty).unwrap();
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(FLIGHT_STATE_PERIOD_MS * 2 + 200) {
        if let Some(packet) = _next_state_aware_packet().unwrap() {
            assert_ne!(
                packet.data_type(),
                ty,
                "initial periodic Idle cannot become a new FC origin"
            );
        }
        assert!(sim().lock().unwrap().automatic_idle_volatile);
        assert!(variables::automatic_idle_test_snapshot().durable_unchanged(&before));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(router.get_cached_network_variable(ty).unwrap().is_none());
    assert!(prepared.try_guard().is_some());

    // Existing owned command/actual model algorithm, not a forced flight flag.
    let generation = crate::gse::capture_operator_origin(&state).unwrap();
    let _claim = crate::gse::claim_operator_dispatch(&state, generation).unwrap();
    sync_local_flight_state(FlightState::Armed);
    assert!(handle_command(&TelemetryCommand::Launch));
    let start = sim().lock().unwrap().launch_sequence_started_ms.unwrap();
    {
        let mut model = sim().lock().unwrap();
        assert!(!model.automatic_idle_volatile);
        assert_eq!(model.flight_state, FlightState::Launch);
        // Invoke the actual deterministic sequence at its existing countdown
        // boundary; no production clock/rate/limit is changed by this unit.
        model.update_launch_sequence(start + LAUNCH_COUNTDOWN_DURATION_MS);
        assert_eq!(model.flight_state, FlightState::Ascent);
        assert!(model.valve_on(ValveBoardCommands::PilotOpen as u8));
        assert!(
            model.queued.iter().any(|q| q.data_type() == ty
                && q.payload().first() == Some(&(FlightState::Ascent as u8)))
        );
    }
    assert!(
        prepared.try_guard().is_none(),
        "ordinary readiness cannot inhibit/replace accepted flight sequence"
    );
}
