// Actual coordinator/HTTP/public-SDK owners; controlled observer contention is
// distinct from final_proof_clock's separately held actual source-I/O case.
#[derive(Clone, Copy)]
enum ClockFinalScenario { Release, InvalidThenGood, Stop, Expired }
async fn actual_clock_final_scenario(scenario: ClockFinalScenario) {
    let fixture = Fixture::new().await;
    let state = fixture.state();
    *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
    for board in BOARDS { fixture.query(board).await; }
    let ticket = fixture.prepare("reset_startup", None).await;
    let ready = PauseGuard::install(&CLOCK_READY, &state);
    let busy = Arc::new(tokio::sync::Notify::new());
    struct BusyReceipt(usize);
    impl Drop for BusyReceipt {
        fn drop(&mut self) {
            let mut slot = CLOCK_BUSY.lock().unwrap();
            if slot.as_ref().is_some_and(|(key,_)|*key==self.0) { *slot=None; }
        }
    }
    let _busy_guard=BusyReceipt(Arc::as_ptr(&state) as usize);
    *CLOCK_BUSY.lock().unwrap()=Some((Arc::as_ptr(&state) as usize,busy.clone()));
    assert_eq!(fixture.confirm(&ticket).await.status(),StatusCode::OK);
    ready.entered().await;
    let original = state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert!(original.verified && !original.published);
    let owner=state.operator_admission_barrier.load(Ordering::Acquire);
    let generation=crate::gse::automatic_generation(&state);
    let original_requests = || fixture.models.iter().flat_map(|(_,model)| {
        model.lock().unwrap().requests.iter().map(|packet|(packet.packet_id(),packet.timestamp())).collect::<Vec<_>>()
    }).collect::<std::collections::BTreeSet<_>>();
    let requests=original_requests();
    let observed = crate::final_proof_clock::begin_model_observation();
    ready.release();
    tokio::time::timeout(Duration::from_secs(3),busy.notified()).await.unwrap();
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire),owner);
    assert_eq!(crate::gse::automatic_generation(&state),generation);
    assert!(!state.launch_history.lock().unwrap().recovery_pending().unwrap().published);
    assert!(state.board_recovery.coordinator.operation().is_some_and(|op|op["completed"]==false));
    assert_eq!(original_requests(),requests,"Busy must not issue a new board query/intent or timestamp");
    let path=std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    match scenario {
        ClockFinalScenario::Release => (),
        ClockFinalScenario::InvalidThenGood => {
            std::fs::write(&path,"").unwrap();
            assert_eq!(crate::simulation_clock_ms(),Some(100000));
            std::fs::write(&path,"100000").unwrap();
            assert_eq!(crate::simulation_clock_ms(),Some(100000));
        }
        ClockFinalScenario::Stop => { state.manual_abort_intent.request(); }
        ClockFinalScenario::Expired => {
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
    }
    observed.finish(100000,Some(100000));
    let operation=fixture.terminal().await;
    let marker=state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert_eq!(marker.owner,original.owner);
    assert_eq!(marker.generation,original.generation);
    assert_eq!(marker.origin_utc_ms,original.origin_utc_ms);
    assert_eq!(original_requests(),requests,"Retry must retain original SDK identities/timestamps, not requery");
    assert_eq!(crate::gse::automatic_generation(&state),generation);
    assert!(fixture.models.iter().all(|(_,model)|model.lock().unwrap().applied==1));
    match scenario {
        ClockFinalScenario::Release => {
            assert_eq!(operation["success"],true,"{operation}");
            assert!(marker.published);
            assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire),0);
        }
        _ => {
            assert_eq!(operation["success"],false,"{operation}");
            assert!(!marker.published);
            assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire),owner);
        }
    }
}
#[tokio::test]
async fn actual_all_seven_clock_busy_retries_same_owner_without_requery_or_restamp() {
    if crate::operator_queue::isolated_case_with_capture_clock(module_path!(),"actual_all_seven_clock_busy_retries_same_owner_without_requery_or_restamp") {return;}
    actual_clock_final_scenario(ClockFinalScenario::Release).await;
}
#[tokio::test]
async fn actual_all_seven_clock_invalid_then_good_never_revives_original_owner() {
    if crate::operator_queue::isolated_case_with_capture_clock(module_path!(),"actual_all_seven_clock_invalid_then_good_never_revives_original_owner") {return;}
    actual_clock_final_scenario(ClockFinalScenario::InvalidThenGood).await;
}
#[tokio::test]
async fn actual_all_seven_stop_before_clock_retry_cannot_reopen_or_refresh_owner() {
    if crate::operator_queue::isolated_case_with_capture_clock(module_path!(),"actual_all_seven_stop_before_clock_retry_cannot_reopen_or_refresh_owner") {return;}
    actual_clock_final_scenario(ClockFinalScenario::Stop).await;
}
#[tokio::test]
async fn actual_all_seven_clock_retry_has_one_bounded_100ms_contention_budget() {
    if crate::operator_queue::isolated_case_with_capture_clock(module_path!(),"actual_all_seven_clock_retry_has_one_bounded_100ms_contention_budget") {return;}
    actual_clock_final_scenario(ClockFinalScenario::Expired).await;
}
#[tokio::test]
async fn actual_all_seven_slow_verified_marker_preserves_legacy_final_completion_after_five_seconds() {
    if crate::operator_queue::isolated_case_with_capture_clock(module_path!(),"actual_all_seven_slow_verified_marker_preserves_legacy_final_completion_after_five_seconds") {return;}
    let fixture=Fixture::new().await;
    let state=fixture.state();
    *state.state.lock().unwrap()=crate::types::FlightState::Aborted;
    for board in BOARDS {fixture.query(board).await;}
    let ticket=fixture.prepare("reset_startup",None).await;
    let marker=PauseGuard::install(&CLOCK_MARKER,&state);
    assert_eq!(fixture.confirm(&ticket).await.status(),StatusCode::OK);
    marker.entered().await;
    assert!(fixture.models.iter().all(|(_,model)|model.lock().unwrap().applied==1));
    let original=state.launch_history.lock().unwrap().recovery_pending().unwrap();
    let writer=crate::web::clock_worker_tests::WriterGuard::install(fixture.http.directory.join("clock.json"));
    marker.release();
    writer.entered().await;
    // A durable writer wait of several seconds (longer than the old 5 s
    // window, shorter than the journal quiescence bound). Final queries happen
    // AFTER this wait, and their own proof remains current.
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(!state.launch_history.lock().unwrap().recovery_pending().unwrap().published);
    writer.release();
    let operation=fixture.terminal().await;
    assert_eq!(operation["success"],true,"{operation}");
    assert!(operation["age_ms"].as_u64().unwrap()>=5000);
    let finished=state.launch_history.lock().unwrap().recovery_pending().unwrap();
    assert_eq!(finished.owner,original.owner);
    assert_eq!(finished.origin_utc_ms,original.origin_utc_ms);
    assert!(finished.published);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire),0);
    assert!(fixture.models.iter().all(|(_,model)|model.lock().unwrap().applied==1));
}
