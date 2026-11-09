// Bookkeeping assertions use controlled Instants; SDK non-admission below is
// genuine. This does not claim request_board_query metadata preparation is nonblocking.
#[test]
fn actual_sdk_busy_uses_short_gate_without_spending_network_budget_or_replaying_acceptance() {
    use std::sync::{Condvar, atomic::AtomicBool};
    let pause = Arc::new((Mutex::new(false), Condvar::new()));
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }
    let release = Release(pause.clone());
    let (entered, reached) = std::sync::mpsc::channel();
    let armed = Arc::new(AtomicBool::new(false));
    let clock_armed = armed.clone();
    let clock_pause = pause.clone();
    let router = Arc::new(Router::new_with_clock(
        Default::default(),
        Box::new(move || {
            if clock_armed.load(Ordering::SeqCst)
                && std::thread::current().name() == Some("readonly-core-holder")
            {
                let _ = entered.send(());
                let mut done = clock_pause.0.lock().unwrap();
                while !*done {
                    done = clock_pause.1.wait(done).unwrap();
                }
            }
            10
        }),
    ));
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    router.enable_managed_variable(ty).unwrap();
    let packet = Packet::new(
        ty,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "GS",
        1,
        Arc::from([0u8]),
    )
    .unwrap();
    armed.store(true, Ordering::SeqCst);
    let owner = router.clone();
    let seed = packet.clone();
    let holder = std::thread::Builder::new()
        .name("readonly-core-holder".into())
        .spawn(move || owner.seed_managed_variable(seed))
        .unwrap();
    reached.recv_timeout(Duration::from_secs(2)).unwrap();
    let at = 0;
    let now = WallInstant::now();
    let previous = now - Duration::from_secs(3);
    let mut refresh = ReadonlyQueryRefresh::default();
    refresh.last[at] = Some(previous);
    for elapsed in [0, 250] {
        let reason = router.try_tx_queue(packet.clone()).unwrap_err();
        assert!(matches!(
            reason,
            sedsnet::TelemetryError::Io("router core busy")
        ));
        let response = super::super::super::rejected_query_offer(reason);
        assert!(
            response
                .extensions()
                .get::<super::super::super::QueryAdmissionCoreBusy>()
                .is_some()
        );
        let sampled = now + Duration::from_millis(elapsed);
        assert!(!refresh.record_response(at, sampled, &response));
        assert_eq!(refresh.last[at], Some(previous));
        assert!(!refresh.due(at, sampled + Duration::from_millis(249)));
        assert!(refresh.due(at, sampled + Duration::from_millis(250)));
    }
    drop(release);
    holder.join().unwrap().unwrap();
    router.try_tx_queue(packet).unwrap();
    assert_eq!(router.try_has_pending_reliable_tx_type(ty), Ok(true));
    let accepted_at = now + Duration::from_millis(500);
    assert!(refresh.record_response(at, accepted_at, &StatusCode::OK.into_response()));
    assert!(!refresh.due(at, accepted_at + Duration::from_millis(1999)));
    assert!(refresh.due(at, accepted_at + Duration::from_millis(2000)));
    assert_eq!(
        router.try_has_pending_reliable_tx_type(ty),
        Ok(true),
        "budget observation never replays or drains the genuine accepted owner"
    );
}
#[test]
fn generic_503_and_owner_409_keep_the_existing_two_second_attempt_cooldown() {
    for response in [
        super::super::super::rejected_query_offer(sedsnet::TelemetryError::Io("other refusal")),
        StatusCode::CONFLICT.into_response(),
    ] {
        let mut refresh = ReadonlyQueryRefresh::default();
        let now = WallInstant::now();
        assert!(!refresh.record_response(0, now, &response));
        assert_eq!(refresh.last[0], Some(now));
        assert!(refresh.core_busy_last[0].is_none());
        assert!(!refresh.due(0, now + Duration::from_millis(1999)));
        assert!(refresh.due(0, now + Duration::from_millis(2000)));
    }
}
#[tokio::test]
async fn actual_sdk_poll_skips_cooling_boards_and_advances_round_robin_after_each_attempt() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "actual_sdk_poll_skips_cooling_boards_and_advances_round_robin_after_each_attempt",
    ) {
        return;
    }
    let fixture = Fixture::new().await;
    let state = fixture.state();
    for board in BOARDS {
        state
            .network_health
            .lock()
            .unwrap()
            .observe(board.sender_id(), WallInstant::now(), 100);
    }
    let mut refresh = ReadonlyQueryRefresh::default();
    let now = WallInstant::now();
    refresh.core_busy_last[0] = Some(now);
    refresh.last[1] = Some(now);
    // Test-fire has no AV bay: RF, FC and PB are not network peers there, so
    // the first eligible peer after the cooling ones is AB (index 3).
    let first = if crate::runtime_mode::is_test_fire() { 3 } else { 2 };
    let _ = refresh.poll(&state);
    assert_eq!(
        refresh.cursor, first + 1,
        "first eligible original peer is the next tracked board, whether actual SDK accepts or is Busy"
    );
    let _ = refresh.poll(&state);
    assert_eq!(
        refresh.cursor, first + 2,
        "the next peer gets the next turn; Busy never monopolizes selection"
    );
}
#[tokio::test]
async fn real_unrefreshed_query_still_expires_at_the_readback_ttl() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "real_unrefreshed_query_still_expires_at_the_readback_ttl",
    ) {
        return;
    }
    let fixture = Fixture::new().await;
    fixture.query(Board::ActuatorBoard).await;
    let state = fixture.state();
    let at = index(Board::ActuatorBoard).unwrap();
    let original = state.board_recovery.inner.lock().unwrap().values[at]
        .clone()
        .unwrap();
    assert!(original.fresh(Stamp::now()));
    tokio::time::sleep(READBACK_TTL + Duration::from_millis(100)).await;
    assert!(
        !original.fresh(Stamp::now()),
        "scheduler cannot refresh an actual old query by bookkeeping or receipt time"
    );
}
