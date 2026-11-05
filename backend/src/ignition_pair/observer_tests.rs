//! Observer facets, not board-presence or physical-actuation qualification.
//! Routing metadata is produced by an actual SDK peer, never a seeded mask.
use super::*;
use sedsnet::router::{EndpointHandler, RouterConfig};

fn actual_peer_router() -> (Arc<Router>, usize) {
    crate::telemetry_schema::initialize().unwrap();
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    let side = host.add_side_packet("actual-observer-peer", |_| Ok(()));
    let wire = Arc::new(Mutex::new(Vec::new()));
    let capture = wire.clone();
    let peer = Router::new(
        RouterConfig::new([
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                |_| Ok(()),
            ),
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("VALVE_BOARD"),
                |_| Ok(()),
            ),
        ])
        .with_sender("TEST_GATEWAY"),
    );
    peer.add_side_packet("host", move |packet| {
        capture.lock().unwrap().push(packet.clone());
        Ok(())
    });
    peer.announce_discovery().unwrap();
    peer.process_all_queues_with_timeout(0).unwrap();
    for packet in std::mem::take(&mut *wire.lock().unwrap()) {
        host.rx_from_side(&packet, side).unwrap();
    }
    assert!(host.export_topology().routes.iter().any(|route| {
        route
            .reachable_endpoints
            .contains(&crate::telemetry_schema::endpoint("ACTUATOR_BOARD"))
            && route
                .reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint("VALVE_BOARD"))
    }));
    (host, side)
}
fn completed(owner: &Observer) {
    let until = Instant::now() + Duration::from_secs(2);
    while owner.in_flight.load(Ordering::Acquire) {
        assert!(
            Instant::now() < until,
            "single actual observer job must complete"
        );
        std::thread::yield_now();
    }
}
fn causal_export_change(reconnect: bool) {
    let (router, side) = actual_peer_router();
    let owner = Arc::new(Observer::default());
    let connection = Arc::new(AtomicU64::new(1));
    owner.register_connection(side, Some(connection.clone()));
    let pause = super::test_pause::Pause::install(&owner);
    let issued = Instant::now();
    owner.request(router.clone(), 0);
    pause.entered.recv_timeout(Duration::from_secs(2)).unwrap();
    if reconnect {
        connection.store(0, Ordering::Release);
        connection.store(2, Ordering::Release);
    } else {
        owner.io_error(side);
    }
    pause.resume.send(()).unwrap();
    completed(&owner);
    assert!(
        issued.elapsed() < MAX_AGE,
        "causal control must finish within the real age window, so TTL expiry cannot explain refusal"
    );
    assert!(
        owner.read(&router, 0).is_err(),
        "old exported routes must not acquire newer local ownership after capture"
    );
    drop(pause);
    owner.request(router.clone(), 0);
    completed(&owner);
    assert!(
        owner.read(&router, 0).is_ok(),
        "a separate actual fresh observation may prove the restored context"
    );
}
#[test]
fn causal_reconnect_during_actual_sdk_exports_does_not_restamp_old_route() {
    causal_export_change(true);
}
#[test]
fn causal_local_error_during_actual_sdk_exports_does_not_restamp_old_route() {
    causal_export_change(false);
}
#[tokio::test]
async fn causal_invalid_input_and_data_do_not_hard_latch_an_unsupported_side() {
    for kind in [
        std::io::ErrorKind::InvalidInput,
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::WouldBlock,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::TimedOut,
    ] {
        let state = crate::state::tests::test_app_state().await;
        let watcher = SideWatcher::new(&state, 0);
        watcher.send_error(&std::io::Error::from(kind));
        assert_eq!(
            state
                .ignition_observer
                .unsupported_faults
                .load(Ordering::Acquire),
            0,
            "parser/caller/backpressure errors are not proof of a dead physical connection: {kind:?}"
        );
    }
}
#[test]
fn late_capture_cannot_publish_as_fresh_and_reads_never_renew_capture_age() {
    let (router, side) = actual_peer_router();
    let owner = Arc::new(Observer::default());
    owner.register_connection(side, Some(Arc::new(AtomicU64::new(1))));
    let pause = super::test_pause::Pause::install(&owner);
    owner.request(router.clone(), 0);
    pause.entered.recv_timeout(Duration::from_secs(2)).unwrap();
    std::thread::sleep(MAX_AGE + Duration::from_millis(10));
    pause.resume.send(()).unwrap();
    completed(&owner);
    assert!(
        owner.cache.lock().unwrap().is_none(),
        "late completion cannot renew request-time age"
    );
    drop(pause);
    owner.request(router.clone(), 0);
    completed(&owner);
    let token = owner.read(&router, 0).unwrap();
    let original = token.requested;
    assert_eq!(owner.read(&router, 0).unwrap().requested, original);
    std::thread::sleep(MAX_AGE + Duration::from_millis(10));
    assert!(owner.read(&router, 0).is_err());
    assert_eq!(
        owner.cache.lock().unwrap().as_ref().unwrap().requested,
        original
    );
}
#[test]
fn live_connection_fences_are_side_bound_and_old_tokens_cannot_cross_reconnects() {
    let (router, side) = actual_peer_router();
    let owner = Arc::new(Observer::default());
    let failed = Arc::new(AtomicU64::new(1));
    let alternate = Arc::new(AtomicU64::new(1));
    owner.register_connection(side, Some(failed.clone()));
    owner.request(router.clone(), 0);
    completed(&owner);
    let token = owner.read(&router, 0).unwrap();
    failed.store(0, Ordering::Release);
    assert!(!owner.valid(&token, &router, 0));
    alternate.store(2, Ordering::Release);
    assert!(
        !owner.valid(&token, &router, 0),
        "another side or a reply cannot clear the failed connection's atomic"
    );
    assert_eq!(failed.load(Ordering::Acquire), 0);
    failed.store(2, Ordering::Release);
    assert!(
        !owner.valid(&token, &router, 0),
        "old generation never gains new-session authority"
    );
    owner.request(router.clone(), 0);
    completed(&owner);
    assert!(owner.read(&router, 0).is_ok());
    owner.invalidate();
    assert!(owner.read(&router, 0).is_err());
}
#[test]
fn context_overflow_and_unknown_side_fail_closed() {
    let (router, _) = actual_peer_router();
    let owner = Arc::new(Observer::default());
    owner.epoch.store(u64::MAX, Ordering::Release);
    owner.invalidate();
    assert!(owner.exhausted.load(Ordering::Acquire));
    assert!(owner.read(&router, 0).is_err());
    let owner = Observer::default();
    owner.side_versions[0].store(u64::MAX, Ordering::Release);
    owner.io_error(0);
    assert!(owner.exhausted.load(Ordering::Acquire));
    let owner = Observer::default();
    owner.register_connection(MAX_SIDES, Some(Arc::new(AtomicU64::new(1))));
    assert!(owner.exhausted.load(Ordering::Acquire));
}
