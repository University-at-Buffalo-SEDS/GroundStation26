//! Production refresh + actual packed SDK peer; no Settings polling or SET.
use super::*;
use std::time::Duration;

#[tokio::test]
async fn background_queries_keep_drive_and_board_fresh_without_settings_polling() {
    let mut fixture = http_tests::Fixture::new().await;
    let mut refresh = crate::board_abort::ActuatorReadbackRefresh::default();
    // Unknown/unseen peers must not be probed, even with discovered endpoints.
    assert!(!refresh.poll(&fixture.state));
    let started = Instant::now();
    let mut observations = 0;
    while started.elapsed() < Duration::from_secs(10) {
        fixture.state.network_health.lock().unwrap().observe("AB", Instant::now(), 100);
        let _ = refresh.poll(&fixture.state);
        fixture.pump();
        let snapshot = view(&fixture.state, true);
        assert_eq!(snapshot["board"]["fresh"], true, "{snapshot}");
        assert_eq!(snapshot["can_configure"], true, "{snapshot}");
        assert!(crate::board_recovery::actuator_configuration_proof(&fixture.state).is_some());
        {
            let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
            assert!(inner.requests.iter().all(|row| row.request.mode.is_none()));
            assert!(inner.pending_setting.is_none());
            assert_eq!(inner.desired.revision, 0);
        }
        observations += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(observations >= 80);
    // Stop delivery completely: freshness expires instead of retaining the last
    // frame as a fresh control proof. A repeated call cannot re-date it.
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    let _ = refresh.poll(&fixture.state);
    let snapshot = view(&fixture.state, true);
    assert_eq!(snapshot["board"]["fresh"], false);
    assert_eq!(snapshot["can_configure"], false);
    assert!(crate::board_recovery::actuator_configuration_proof(&fixture.state).is_none());
    assert!(fixture.cmd_rx.as_mut().unwrap().try_recv().is_err());
}

#[tokio::test]
async fn pending_drive_nonce_is_not_replaced_or_restamped_by_background_query() {
    let fixture = http_tests::Fixture::new().await;
    request_drive_query(&fixture.state).unwrap();
    let (nonce, sent) = {
        let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
        assert_eq!(inner.requests.len(), 1);
        (inner.requests[0].request.nonce, inner.requests[0].sent.wall)
    };
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    request_drive_query(&fixture.state).unwrap();
    let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    assert_eq!(inner.requests.len(), 1);
    assert_eq!(inner.requests[0].request.nonce, nonce);
    assert_eq!(inner.requests[0].sent.wall, sent);
    assert!(inner.requests[0].request.mode.is_none());
}

#[tokio::test]
async fn background_drive_query_preserves_unanswered_set_and_its_pending_nonce() {
    let fixture = http_tests::Fixture::new().await;
    {
        let mut inner = fixture.state.plumbing_drive.inner.lock().unwrap();
        let mut sent = Moment::now();
        sent.wall -= Duration::from_millis(1_500);
        if let Some(model) = sent.model.as_mut() { *model = model.checked_sub(1_500).unwrap_or(u64::MAX); }
        inner.requests.push_back(Context {
            request: Request { mode: Some(Mode::Auxiliary), nonce: 55, generation: 42,
                epoch: 1, issuer: fixture.state.plumbing_drive.issuer },
            sent, complete: false, recovery_generation: crate::gse::automatic_generation(&fixture.state),
            desired_revision: 1,
        });
        inner.pending_setting = Some(55);
    }
    request_drive_query(&fixture.state).unwrap();
    let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    assert_eq!(inner.pending_setting, Some(55));
    assert_eq!(inner.requests.len(), 1);
    assert_eq!(inner.requests[0].request.mode, Some(Mode::Auxiliary));
    assert!(!inner.requests[0].complete);
}

#[tokio::test]
async fn motion_query_preserves_pending_current_context_but_refreshes_after_mode_change() {
    let fixture = http_tests::Fixture::new().await;
    request_motion_query(&fixture.state).unwrap();
    let (nonce, sent) = {
        let mut inner = fixture.state.plumbing_drive.inner.lock().unwrap();
        let query = inner.motion_queries.back_mut().unwrap();
        query.sent.wall -= Duration::from_millis(1_500);
        if let Some(model) = query.sent.model.as_mut() { *model = model.checked_sub(1_500).unwrap_or(u64::MAX); }
        (query.nonce, query.sent.wall)
    };
    request_motion_query(&fixture.state).unwrap();
    {
        let mut inner = fixture.state.plumbing_drive.inner.lock().unwrap();
        assert_eq!(inner.motion_queries.len(), 1);
        assert_eq!((inner.motion_queries[0].nonce, inner.motion_queries[0].sent.wall), (nonce, sent));
        inner.desired.revision += 1;
        inner.desired.mode = Mode::Auxiliary as u8;
    }
    request_motion_query(&fixture.state).unwrap();
    let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    assert_eq!(inner.motion_queries.len(), 2);
    assert_ne!(inner.motion_queries[1].nonce, nonce);
    assert_eq!(inner.motion_queries[1].mode_revision, inner.desired.revision);
    assert_eq!(inner.motion_queries[0].sent.wall, sent);
}

#[tokio::test]
async fn drive_mismatch_is_distinct_from_busy_or_unready_feedback() {
    let mut fixture = http_tests::Fixture::new().await;
    fixture.prime().await;
    let mut inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    inner.desired.mode = Mode::Auxiliary as u8;
    let mismatch = motion_ready_inner(&fixture.state, &inner).unwrap_err();
    assert!(mismatch.contains("reports Stepper") && mismatch.contains("selected Auxiliary"), "{mismatch}");
    assert!(!mismatch.contains("moving"));
    inner.observed.as_mut().unwrap().value.busy = true;
    inner.observed.as_mut().unwrap().value.active_action = 1;
    inner.observed.as_mut().unwrap().value.remaining_ms = 123;
    let busy = motion_ready_inner(&fixture.state, &inner).unwrap_err();
    assert!(busy.contains("retracting") && busy.contains("123 ms"), "{busy}");
    inner.observed.as_mut().unwrap().value.busy = false;
    inner.observed.as_mut().unwrap().value.result = 7;
    let unready = motion_ready_inner(&fixture.state, &inner).unwrap_err();
    assert!(unready.contains("not ready"), "{unready}");
}

fn running_retract(remaining_ms: u32, issuer: u64) -> motion_wire::MotionStatus {
    motion_wire::MotionStatus {
        setting: Status {
            result: 0,
            mode: Some(Mode::Auxiliary),
            capabilities: 5,
            nonce: 9,
            generation: 42,
            epoch: 8,
            issuer,
            active_action: 1,
            busy: true,
            remaining_ms,
            setting_id: 0,
        },
        lifecycle: 1,
        accepted_action: 1,
        original_utc_ms: 1_800_000_000_000,
        minimum_delay_remaining_ms: remaining_ms,
        origin_epoch: 8,
    }
}

/// The auxiliary drive reports "retracted" only after its 45 s timed move.
/// An accepted, running retraction keeps the operator's intent and shows as
/// active instead of expiring to Unconfirmed and then "not triggered".
#[tokio::test]
async fn running_auxiliary_retraction_stays_active_until_it_reports_retracted() {
    let fixture = http_tests::Fixture::new().await;
    let state = &fixture.state;
    let key = crate::rocket_commands::ActuatorBoardCommands::RetractPlumbing as u8;
    state.set_umbilical_valve_state(key, false);
    state.set_pending_umbilical_valve_state(key, true);
    assert!(!retract_motion_active(state));
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        let revision = inner.desired.revision;
        inner.motion_observed = Some(MotionObservation {
            value: running_retract(44_000, state.plumbing_drive.issuer),
            after: Moment::now(),
            received: Instant::now(),
            generation: crate::gse::automatic_generation(state),
            mode_revision: revision,
        });
    }
    assert!(retract_motion_active(state));
    // Past the 2 s pending window, with the board still reporting not retracted.
    assert!(!state.expire_pending_umbilical_valve_states(Duration::ZERO));
    assert!(!state.reconcile_pending_umbilical_valve_state(key, false, Duration::ZERO));
    assert_eq!(state.get_pending_umbilical_valve_state(key), Some(true));
    let valves = crate::sequences::retract_actuated_for_test(state);
    assert_eq!(valves, Some(true), "a running retraction shows active");
    // Completion report: the intent resolves to the confirmed retracted state.
    state.set_umbilical_valve_state(key, true);
    // A finished or cancelled motion no longer holds the intent.
    {
        let mut inner = state.plumbing_drive.inner.lock().unwrap();
        let row = inner.motion_observed.as_mut().unwrap();
        row.value.lifecycle = 3;
    }
    assert!(!retract_motion_active(state));
    assert!(state.expire_pending_umbilical_valve_states(Duration::ZERO));
}

#[test]
fn running_retraction_lapses_after_its_reported_remaining_time() {
    let row = MotionObservation {
        value: running_retract(10_000, 7),
        after: Moment::now(),
        received: Instant::now(),
        generation: 3,
        mode_revision: 1,
    };
    assert!(retract_row_active(&row, 3, 1, Duration::from_millis(12_000)));
    assert!(!retract_row_active(&row, 3, 1, Duration::from_millis(13_001)));
    assert!(!retract_row_active(&row, 4, 1, Duration::ZERO), "Stop/recovery generation");
    assert!(!retract_row_active(&row, 3, 2, Duration::ZERO), "drive changed");
}
