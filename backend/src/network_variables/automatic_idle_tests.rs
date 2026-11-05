use super::*;
use sedsnet::router::RouterConfig;

#[test]
fn volatile_automatic_idle_never_leaks_through_config_or_reload() {
    let root = std::env::temp_dir().join(format!("gs-auto-store-{}", rand::random::<u128>()));
    let path = root.join("variables.json");
    let local = Mutex::new(VariableStore::new(path.clone()));
    let baseline = PersistentVariables::default();
    local.lock().unwrap().commit(baseline).unwrap();
    let revision = local.lock().unwrap().revision;
    {
        let mut guard = AutomaticIdleStoreGuard(local.lock().unwrap());
        assert!(guard.matches(AutomaticIdleSnapshot {
            revision,
            state: 0,
            stamp: 0
        }));
        guard.admitted(AutomaticIdleIdentity {
            packet_id: 10,
            timestamp: 20,
            generation: 1,
        });
    }
    let mut next = baseline;
    next.av_bay_underglow = true;
    next.flight_buzzer = true;
    local.lock().unwrap().commit(next).unwrap();
    assert!(local.lock().unwrap().automatic_idle.is_some());
    let restored = VariableStore::new(path.clone());
    assert_eq!(restored.values.flight_state, 0);
    assert_eq!(restored.values.flight_state_stamp, 0);
    assert!(restored.automatic_idle.is_none());
    assert!(restored.values.av_bay_underglow && restored.values.flight_buzzer);
    next.flight_state = 1;
    next.flight_state_stamp = 99;
    local.lock().unwrap().commit_flight_state(next).unwrap();
    assert!(local.lock().unwrap().automatic_idle.is_none());
    assert_eq!(
        VariableStore::new(path.clone()).values,
        next,
        "manual desired Idle remains durable"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn exhausted_automatic_revision_does_not_disable_durable_manual_abort() {
    let root = std::env::temp_dir().join(format!("gs-auto-exhaust-{}", rand::random::<u128>()));
    let path = root.join("variables.json");
    let local = Mutex::new(VariableStore::new(path.clone()));
    local.lock().unwrap().revision = u64::MAX;
    assert!(
        !AutomaticIdleStoreGuard(local.lock().unwrap()).matches(AutomaticIdleSnapshot {
            revision: u64::MAX,
            state: 0,
            stamp: 0
        })
    );
    let mut abort = local.lock().unwrap().values;
    abort.flight_state = 15;
    abort.flight_state_stamp = 123;
    local.lock().unwrap().commit_flight_state(abort).unwrap();
    assert_eq!(VariableStore::new(path).values, abort);
    assert_eq!(local.lock().unwrap().revision, u64::MAX);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn primary_echo_is_observational_but_rrls_and_manual_persistence_remain() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "primary_echo_is_observational_but_rrls_and_manual_persistence_remain",
    ) {
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    let ty = crate::telemetry_schema::data_type(FLIGHT_STATE_TYPE);
    let ep = crate::telemetry_schema::endpoint("FLIGHT_STATE");
    let primary = Router::new(RouterConfig::new([]).with_sender("GS"));
    primary
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    initialize_for_role_seed(&primary, false, false).unwrap();
    let baseline = store().lock().unwrap().values;
    let echo = Packet::new(ty, &[ep], "GS", 10000, Arc::from([1u8])).unwrap();
    primary.rx(&echo).unwrap();
    assert_eq!(
        store().lock().unwrap().values,
        baseline,
        "own old original is not new durable authority"
    );
    assert_eq!(
        primary
            .get_cached_network_variable(ty)
            .unwrap()
            .unwrap()
            .packet_id(),
        echo.packet_id(),
        "SDK may retain accepted original provenance"
    );
    let restarted = Router::new(RouterConfig::new([]).with_sender("GS"));
    initialize_for_role_seed(&restarted, false, false).unwrap();
    restarted.rx(&echo).unwrap();
    assert_eq!(
        store().lock().unwrap().values,
        baseline,
        "new router cannot promote delayed old echo"
    );
    let observer = Router::new(RouterConfig::new([]).with_sender("RRLS"));
    initialize_for_role_seed(&observer, true, false).unwrap();
    let paired = Packet::new(ty, &[ep], "GS", 10001, Arc::from([2u8])).unwrap();
    observer.rx(&paired).unwrap();
    assert_eq!(
        flight_state(),
        2,
        "RRLS still records actual paired primary observation"
    );
    set_flight_state(&primary, 1).unwrap();
    assert_eq!(
        VariableStore::new(cache_path()).values.flight_state,
        1,
        "explicit manual setter remains durable"
    );
}

#[cfg(feature = "test_fire_mode")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_request_shutdown_retires_unadmitted_automatic_original() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_request_shutdown_retires_unadmitted_automatic_original",
    ) {
        return;
    }
    struct Release(std::sync::Arc<crate::recovery_pending_probe::Probe>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    crate::telemetry_schema::initialize().unwrap();
    let _ = flight_state();
    let state = crate::state::tests::test_app_state().await;
    assert_eq!(
        state.local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_000_000_000,
        ))
        .unwrap();
    initialize_for_role_seed(&router, false, false).unwrap();
    state.topology_router.set(router.clone()).unwrap();
    let probe = crate::recovery_pending_probe::Probe::new(None);
    probe.bind(&state);
    let release = Release(probe.clone());
    probe
        .block_commit
        .store(true, std::sync::atomic::Ordering::Release);
    let before = automatic_idle_test_snapshot();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let cache = router.get_cached_network_variable(ty).unwrap();
    let original = crate::automatic_startup_idle::Captured::capture(&state, true).unwrap();
    let owner_state = state.clone();
    let owner = tokio::spawn(async move {
        crate::automatic_startup_idle::publish(&owner_state, original).await;
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        probe.commit_entered.notified(),
    )
    .await
    .unwrap();
    assert!(
        state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    );
    assert!(
        state.request_shutdown(),
        "actual shutdown API sets atomic and signals, not a fabricated test flag"
    );
    probe.release();
    drop(release);
    tokio::time::timeout(std::time::Duration::from_secs(3), owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        state.local_flight_state_snapshot(),
        crate::types::FlightState::Startup
    );
    assert_eq!(automatic_idle_test_snapshot(), before);
    assert_eq!(router.get_cached_network_variable(ty).unwrap(), cache);
    assert!(!router.has_pending_reliable_tx_type(ty));
    assert_eq!(
        state
            .operator_dispatch_inflight
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    crate::recovery_pending_probe::clear_for_state(&state);
}
