// Native actual selector -> actual public SDK admission; no physical outputs.
use super::*;
use crate::automatic_flight_state::Observation;

#[tokio::test]
async fn healthy_original_steps_keep_all_normal_prelaunch_phases_live_only() {
    crate::mode_suite!(Normal, Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "healthy_original_steps_keep_all_normal_prelaunch_phases_live_only",
    ) {
        return;
    }
    let (state, router) = crate::automatic_flight_state::tests::ready().await;
    for board in crate::types::Board::ALL {
        state.mark_board_seen(board.sender_id(), 1);
    }
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let mut runtime = SequenceRuntime::for_generation(crate::gse::automatic_generation(&state));
    let valves = ValveSnapshot {
        normally_open: Some(true),
        dump_open: Some(false),
        ..Default::default()
    };
    for (step, expected) in [
        (SequenceStep::SetupValves, FlightState::Idle),
        (SequenceStep::SetupValves, FlightState::PreFill),
        (SequenceStep::NitrogenFill, FlightState::NitrogenFill),
        (SequenceStep::NitrogenLeakCheck, FlightState::FillTest),
        (SequenceStep::OpenNitrous, FlightState::NitrousFill),
        (SequenceStep::ArmedReady, FlightState::Armed),
    ] {
        runtime.step = step;
        runtime.calibration_ready = true;
        let observation = Observation::begin(&state).unwrap();
        let current = state.local_flight_state_snapshot();
        assert_eq!(
            drive_observed_prelaunch_state(
                &state,
                &runtime,
                valves,
                current,
                observation.captured()
            ),
            expected
        );
        assert_eq!(
            router
                .get_cached_network_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
                .unwrap()
                .unwrap()
                .payload(),
            &[expected.network() as u8],
            "the wire carries the network value; the sub-phase stays in the station"
        );
        assert!(
            crate::network_variables::automatic_idle_test_snapshot().durable_unchanged(&before)
        );
    }
}
#[tokio::test]
async fn actual_http_retained_stop_then_actual_refresh_cannot_publish_idle() {
    crate::mode_suite!(Normal, Testing);
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_retained_stop_then_actual_refresh_cannot_publish_idle",
    ) {
        return;
    }
    let _ = crate::network_variables::flight_state();
    let fixture = crate::actuator_plumbing::http_tests::Fixture::new().await;
    let state = fixture.state.clone();
    // Explicit fixture state setup before the tested original, using the
    // existing genuine explicit setter rather than injected epoch/readiness.
    state.set_local_flight_state(FlightState::Startup);
    for board in crate::types::Board::ALL {
        state.mark_board_seen(board.sender_id(), 1);
    }
    let before = crate::network_variables::automatic_idle_test_snapshot();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let old_packet = fixture.host.get_cached_network_variable(ty).unwrap();
    let old = old_packet.as_ref().map(|p| p.packet_id());
    let before_packet = old_packet.as_ref().map(|packet| serde_json::json!({
        "id":packet.packet_id(),"sourceUTC":packet.timestamp(),
        "sender":packet.sender(),"type":packet.data_type().as_u32(),"payload":packet.payload()
    }));
    let request_started = std::time::Instant::now();
    let response = tokio::time::timeout(
        Duration::from_millis(100),
        fixture.http(
            "POST",
            "/api/command",
            Some("operator"),
            serde_json::json!("Abort"),
        ),
    )
    .await;
    if response.is_err() {
        // The ORIGINAL100ms bound already elapsed and dropped the owned HTTP
        // future. Observe only; no second auth/command, await or SDK cache read.
        let current_state = state.state.try_lock()
            .map(|value| format!("{value:?}"))
            .unwrap_or_else(|error| format!("unavailable:{error:?}"));
        let diagnostic = serde_json::json!({
            "elapsed_us":request_started.elapsed().as_micros(),
            "state_try_snapshot":current_state,
            "pendingStop":state.manual_abort_intent.pending_epoch(),
            "manualEpoch":state.manual_abort_intent.epoch(),
            "automaticGeneration":crate::gse::automatic_generation(&state),
            "operator_inflight":state.operator_dispatch_inflight.load(std::sync::atomic::Ordering::Acquire),
            "shutdown_requested":state.shutdown_requested.load(std::sync::atomic::Ordering::Acquire),
            "auth_pool_size":state.auth_db.size(),"auth_pool_idle":state.auth_db.num_idle(),
            "gse_try_available":state.gse.try_lock().is_ok(),
            "before_packet":before_packet,
            "after_packet":"not_sampled_no_nonblockingSDKcachegetter",
            "snapshot_scope":"post_timeout/non_atomic; no HTTPphase/Stopabsence/handlercontinuation proof"
        });
        eprintln!("ABORT_HTTP100_DIAGNOSTIC {diagnostic}");
    }
    let response = response.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let epoch = state.manual_abort_intent.pending_epoch().unwrap();
    refresh_action_policy_now(&state);
    assert_eq!(state.manual_abort_intent.pending_epoch(), Some(epoch));
    assert_eq!(state.local_flight_state_snapshot(), FlightState::Startup);
    assert_eq!(
        fixture
            .host
            .get_cached_network_variable(ty)
            .unwrap()
            .map(|p| p.packet_id()),
        old
    );
    assert_eq!(
        crate::network_variables::automatic_idle_test_snapshot(),
        before
    );
}
