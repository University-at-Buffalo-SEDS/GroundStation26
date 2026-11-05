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
        let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
        assert!(inner.requests.iter().all(|row| row.request.mode.is_none()));
        assert!(inner.pending_setting.is_none());
        assert_eq!(inner.desired.revision, 0);
        drop(inner);
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
