//! Actual SDK owner/receipt protocol through the native UART transport boundary.
use super::*;
#[cfg(target_os = "linux")]
use sedsnet::router::WriteStart;
use sedsnet::router::{
    EndpointHandler, PackedWriteLease, RetryTracker, RetryTrackerLimits, RouterConfig,
    RouterSideOptions,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

pub(crate) struct Fixture {
    pub router: Router,
    pub tracker: RetryTracker,
    pub leases: Arc<Mutex<Vec<PackedWriteLease>>>,
    side: RouterSideId,
    now: Arc<AtomicU64>,
    endpoint: sedsnet::config::DataEndpoint,
    types: [sedsnet::config::DataType; 3],
}
impl Fixture {
    pub fn new(admit: bool) -> Self {
        crate::telemetry_schema::initialize().unwrap();
        let endpoint = crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER");
        static TYPES: std::sync::OnceLock<[sedsnet::config::DataType; 3]> =
            std::sync::OnceLock::new();
        let types = *TYPES.get_or_init(|| {
            ["GS_RETRY_TEST_1", "GS_RETRY_TEST_2", "GS_RETRY_TEST_3"].map(|name| {
                sedsnet::config::register_data_type(
                    name,
                    sedsnet::MessageElement::Static(
                        1,
                        sedsnet::MessageDataType::UInt8,
                        sedsnet::MessageClass::Data,
                    ),
                    &[endpoint],
                    sedsnet::ReliableMode::Ordered,
                    200,
                )
                .unwrap()
            })
        });
        let now = Arc::new(AtomicU64::new(1000));
        let clock = now.clone();
        let router = Router::new_with_clock(
            RouterConfig::default()
                .with_sender("GS")
                .with_reliable_enabled(true),
            Box::new(move || clock.load(Ordering::Acquire)),
        );
        let leases = Arc::new(Mutex::new(Vec::new()));
        let output = leases.clone();
        let (side, tracker) = router
            .add_side_packed_leased_with_priority_and_options(
                "radio",
                move |lease, _| {
                    if serialize::peek_envelope(lease.bytes())
                        .is_ok_and(|env| types.contains(&env.ty))
                    {
                        if admit {
                            lease.admit()?;
                        }
                        output.lock().unwrap().push(lease);
                    }
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: false,
                    header_template_enabled: false,
                    ..Default::default()
                },
                RetryTrackerLimits::default(),
            )
            .unwrap();
        let topology = sedsnet::discovery::build_discovery_topology(
            "RF",
            0,
            &[sedsnet::discovery::TopologyBoardNode {
                sender_id: "RF".into(),
                reachable_endpoints: vec![endpoint],
                reachable_timesync_sources: vec![],
                connections: vec!["GS".into()],
            }],
        )
        .unwrap();
        router.rx_from_side(&topology, side).unwrap();
        router.clear_tx_queue();
        Self {
            router,
            tracker,
            leases,
            side,
            now,
            endpoint,
            types,
        }
    }
    pub fn publish(&self, value: u8) -> PackedWriteLease {
        let before = self.leases.lock().unwrap().len();
        self.router
            .tx(Packet::new(
                self.types[usize::from(value.saturating_sub(1)).min(2)],
                &[self.endpoint],
                "GS",
                self.now.load(Ordering::Acquire),
                Arc::from([value]),
            )
            .unwrap())
            .unwrap();
        self.leases
            .lock()
            .unwrap()
            .get(before)
            .expect("actual SDK first dispatch")
            .clone()
    }
    pub fn retry(&self) -> PackedWriteLease {
        let before = self.leases.lock().unwrap().len();
        self.now.fetch_add(1000, Ordering::AcqRel);
        self.router.process_tx_queue().unwrap();
        self.leases
            .lock()
            .unwrap()
            .get(before)
            .expect("actual SDK timeout retry")
            .clone()
    }
    pub fn finish(&self, wire: &[u8]) {
        let remote = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(self.endpoint, |_| {
                Ok(())
            })])
            .with_sender("RF"),
            Box::new(|| 0),
        );
        let receipts = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let output = receipts.clone();
        let side = remote.add_side_packed("return", move |bytes| {
            if serialize::peek_envelope(bytes)
                .is_ok_and(|env| env.ty == sedsnet::config::DataType::ReliableAck)
            {
                output.lock().unwrap().push(bytes.to_vec());
            }
            Ok(())
        });
        remote.rx_packed_from_side(wire, side).unwrap();
        remote.dispatch_tx_queue_with_timeout(0).unwrap();
        let receipts = receipts.lock().unwrap();
        assert!(
            !receipts.is_empty(),
            "successful actual remote owner emits ACK"
        );
        for receipt in receipts.iter() {
            self.router.rx_packed_from_side(receipt, self.side).unwrap();
        }
    }
}
#[cfg(target_os = "linux")]
fn uart() -> (serialport::TTYPort, UartComms) {
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(15)).unwrap();
    let (mut uart, _, _) = super::raw_uart_tests::recovery_uart_fixture(port);
    uart.recovery = None;
    (peer, uart)
}
#[cfg(target_os = "linux")]
fn drain(peer: &mut serialport::TTYPort, lease: &PackedWriteLease) {
    let expected = build_raw_uart_frame(lease.bytes()).unwrap();
    let mut actual = vec![0; expected.len()];
    peer.read_exact(&mut actual).unwrap();
    assert_eq!(actual, expected);
}
#[test]
#[cfg(target_os = "linux")]
fn retry_pty_drained_first_allows_final_owner_to_retire_unsent_retry() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    f.finish(first.bytes());
    assert!(retry.discard_if_retired());
    assert!(
        uart.send_tracked(&[(200, retry.bytes())], std::slice::from_ref(&retry))
            .is_err()
    );
    assert!(peer.read(&mut [0]).is_err());
}
#[test]
#[cfg(target_os = "linux")]
fn retry_pty_capacity_preflight_does_not_start_retry() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    uart.enable_radio_capacity_control();
    uart.write_lock
        .lock()
        .unwrap()
        .radio_capacity
        .as_mut()
        .unwrap()
        .committed(Instant::now(), None, u16::MAX as usize);
    assert!(
        uart.send_tracked(&[(200, retry.bytes())], std::slice::from_ref(&retry))
            .is_err()
    );
    f.finish(first.bytes());
    assert!(
        retry.discard_if_retired(),
        "pacing refusal left retry unstarted"
    );
}
#[test]
#[cfg(target_os = "linux")]
fn retry_pty_batch_is_atomic_and_original_group_remains_retriable() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    let second = f.publish(2);
    let frames = [(200, first.bytes()), (200, second.bytes())];
    uart.send_tracked(&frames, &[first.clone(), second.clone()])
        .unwrap();
    drain(&mut peer, &first);
    drain(&mut peer, &second);
    let retry = f.retry();
    f.finish(first.bytes());
    let other = f
        .leases
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|lease| lease.bytes() == second.bytes())
        .unwrap()
        .clone();
    assert!(
        uart.send_tracked(
            &[(200, retry.bytes()), (200, other.bytes())],
            &[retry.clone(), other.clone()]
        )
        .is_err()
    );
    // If the first member is retired, the second retry must remain unstarted.
    f.finish(second.bytes());
    assert!(other.discard_if_retired(), "batch refusal starts no member");
    assert!(peer.read(&mut [0]).is_err());
}
#[test]
#[cfg(target_os = "linux")]
fn retry_binding_is_idempotent_but_peer_epoch_change_releases_old_pruning() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    f.finish(first.bytes());
    // Retired but unconsumed must cease authorizing discard after a new peer epoch.
    uart.observe_retry_peer_session(Some(11));
    uart.observe_retry_peer_session(Some(12));
    assert!(!retry.discard_if_retired());
    assert_eq!(retry.try_start(), WriteStart::Ready);
}

#[test]
#[cfg(target_os = "linux")]
fn retry_pty_partial_write_stays_started_after_final_receipt() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SCALAR_UART_WRITE_BUDGET.with(|slot| slot.set(None));
        }
    }
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    SCALAR_UART_WRITE_BUDGET.with(|slot| slot.set(Some((uart.inner.as_raw_fd(), 4))));
    let reset = Reset;
    assert!(
        uart.send_tracked(&[(200, retry.bytes())], std::slice::from_ref(&retry))
            .is_err()
    );
    let mut prefix = [0; 4];
    peer.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, build_raw_uart_frame(retry.bytes()).unwrap()[..4]);
    drop(reset);
    f.finish(first.bytes());
    assert!(
        !retry.discard_if_retired(),
        "partial physical bytes prevent retry retirement"
    );
    uart.send_tracked(&[(200, retry.bytes())], std::slice::from_ref(&retry))
        .unwrap();
    drain(&mut peer, &retry);
}

#[test]
#[cfg(target_os = "linux")]
fn retry_mesh_local_success_is_not_uart_drain_evidence() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.recovery = Some(Arc::new(Mutex::new(crate::rrls::Mesh::new(
        crate::rrls::Role::Rrls,
        4,
    ))));
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    assert!(
        peer.read(&mut [0]).is_err(),
        "Mesh-local success performed no UART IO"
    );
    let retry = f.retry();
    f.finish(first.bytes());
    assert!(
        !retry.discard_if_retired(),
        "local bookkeeping must not seed physical history"
    );
    assert_eq!(retry.try_start(), WriteStart::Ready);
}

#[test]
#[cfg(target_os = "linux")]
fn retry_pty_three_member_batch_drains_each_canonical_owner() {
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    let second = f.publish(2);
    let third = f.publish(3);
    let leases = [first.clone(), second.clone(), third.clone()];
    uart.send_tracked(
        &[
            (200, first.bytes()),
            (200, second.bytes()),
            (200, third.bytes()),
        ],
        &leases,
    )
    .unwrap();
    for lease in &leases {
        drain(&mut peer, lease);
    }
    let before = f.leases.lock().unwrap().len();
    f.retry();
    let retries = f.leases.lock().unwrap()[before..].to_vec();
    assert_eq!(retries.len(), 3);
    for lease in &leases {
        f.finish(lease.bytes());
    }
    assert!(retries.iter().all(PackedWriteLease::discard_if_retired));
}

#[test]
#[cfg(target_os = "linux")]
fn retry_rebinding_new_router_invalidates_queued_old_tracker_leases() {
    let f = Fixture::new(true);
    let replacement = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    f.finish(first.bytes());
    uart.bind_retry_tracker(replacement.tracker.clone())
        .unwrap();
    assert!(
        !retry.discard_if_retired(),
        "old strong lease handle cannot retain stale pruning authority"
    );
    assert_eq!(retry.try_start(), WriteStart::Ready);
}

#[test]
#[cfg(target_os = "linux")]
fn retry_only_crc_valid_explicit_leave_changes_transport_epoch() {
    for valid in [false, true] {
        let f = Fixture::new(true);
        let (mut peer, mut uart) = uart();
        uart.bind_retry_tracker(f.tracker.clone()).unwrap();
        let first = f.publish(1);
        uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
            .unwrap();
        drain(&mut peer, &first);
        let retry = f.retry();
        f.finish(first.bytes());
        let leave = Packet::new(
            sedsnet::config::DataType::DiscoveryLeave,
            &[sedsnet::config::DataEndpoint::Discovery],
            "RF",
            0,
            Arc::from([]),
        )
        .unwrap();
        let mut bytes = serialize::pack_packet(&leave).to_vec();
        if !valid {
            *bytes.last_mut().unwrap() ^= 1;
        }
        uart.invalidate_retry_on_metadata(&bytes);
        assert_eq!(
            retry.discard_if_retired(),
            !valid,
            "invalid CRC metadata cannot reset progress evidence"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn retry_fallback_partial_io_is_started_without_manufacturing_drain_proof() {
    struct Fallback {
        port: serialport::TTYPort,
    }
    impl CommsDevice for Fallback {
        fn set_side_id(&mut self, _: RouterSideId) {}
        fn recv_packet(&mut self, _: &Router, _: &mut dyn FnMut(&Packet)) -> TelemetryResult<()> {
            Ok(())
        }
        fn send_data(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
            self.port.write_all(&bytes[..2])?;
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into())
        }
    }
    let f = Fixture::new(true);
    let (mut peer, mut uart) = uart();
    uart.bind_retry_tracker(f.tracker.clone()).unwrap();
    let first = f.publish(1);
    uart.send_tracked(&[(200, first.bytes())], std::slice::from_ref(&first))
        .unwrap();
    drain(&mut peer, &first);
    let retry = f.retry();
    let mut fallback = Fallback { port: uart.inner };
    assert!(
        fallback
            .send_tracked(&[(200, retry.bytes())], std::slice::from_ref(&retry))
            .is_err()
    );
    let mut prefix = [0; 2];
    peer.read_exact(&mut prefix).unwrap();
    assert_eq!(prefix, retry.bytes()[..2]);
    f.finish(first.bytes());
    assert!(
        !retry.discard_if_retired(),
        "fallback partial IO must remain started"
    );
}
