// Included lexically in the existing real-adapter wrapper test module.
#[test]
fn primary_from_open_preserves_handle_and_shares_a_real_reconnect_generation() {
    let config = missing().shared.lock().unwrap().config.clone();
    let online = Arc::new(AtomicBool::new(true));
    let ready = Arc::new(AtomicBool::new(true));
    let sent = Arc::new(Mutex::new(Vec::new()));
    let alive = Arc::new(AtomicUsize::new(1));
    let radio = RecoveryRadio::from_open(
        config,
        Box::new(TestRadio {
            online: online.clone(),
            ready: ready.clone(),
            sent: sent.clone(),
            alive: alive.clone(),
        }),
    )
    .unwrap();
    assert_eq!(
        alive.load(Ordering::Acquire),
        2,
        "already-open RX retained; exactly one independent TX clone"
    );
    assert!(
        !radio.shared.lock().unwrap().capacity_enabled,
        "fill UART capacity must not be enabled by wrapping"
    );
    let fence = radio.connection_generation().unwrap();
    let mut tx = radio.try_clone_tx().unwrap().unwrap();
    assert!(Arc::ptr_eq(&fence, &tx.connection_generation().unwrap()));
    assert_eq!(fence.load(Ordering::Acquire), 1);
    ready.store(false, Ordering::Release);
    assert!(tx.send_data(&[1, 2]).is_err());
    assert_eq!(
        fence.load(Ordering::Acquire),
        1,
        "WouldBlock is not connection loss"
    );
    ready.store(true, Ordering::Release);
    tx.send_data_with_priority(&[1, 2], 255).unwrap();
    assert_eq!(
        *sent.lock().unwrap(),
        vec![vec![1, 2]],
        "no opener, AT bytes, retransmission or retuning before the original payload"
    );
    online.store(false, Ordering::Release);
    assert!(tx.send_data(&[3]).is_err());
    assert_eq!(
        fence.load(Ordering::Acquire),
        0,
        "actual BrokenPipe retires the connection synchronously"
    );
    assert_eq!(alive.load(Ordering::Acquire), 0);
    let (o, r, s, a) = (online.clone(), ready.clone(), sent.clone(), alive.clone());
    radio.shared.lock().unwrap().opener = Box::new(move |_| {
        a.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(TestRadio {
            online: o.clone(),
            ready: r.clone(),
            sent: s.clone(),
            alive: a.clone(),
        }))
    });
    online.store(true, Ordering::Release);
    radio.shared.lock().unwrap().retry_at = Instant::now();
    tx.send_data(&[4]).unwrap();
    assert_eq!(fence.load(Ordering::Acquire), 2);
    assert_eq!(*sent.lock().unwrap(), vec![vec![1, 2], vec![4]]);
}
#[test]
fn logical_crc_queue_and_caller_errors_do_not_reconnect_or_hard_classify() {
    for reason in [
        "rx queue busy",
        "router core busy",
        "side tx busy",
        "memory pressure",
        "shared priority queue saturated",
        "no discovered route",
        "reliable window full",
    ] {
        assert!(
            !io_failure(&TelemetryError::Io(reason)),
            "SDK ownership condition is not OS IO failure: {reason}"
        );
    }
    assert!(!io_failure(&TelemetryError::Unpack("crc32 mismatch")));
    for kind in [
        std::io::ErrorKind::WouldBlock,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::InvalidInput,
        std::io::ErrorKind::InvalidData,
    ] {
        assert!(!boxed_io_failure(&std::io::Error::from(kind)), "{kind:?}");
    }
    assert!(boxed_io_failure(&std::io::Error::from(
        std::io::ErrorKind::BrokenPipe
    )));
    assert!(boxed_io_failure(&std::io::Error::from(
        std::io::ErrorKind::NotConnected
    )));
}
#[test]
fn reconnect_generation_overflow_cannot_publish_a_reused_connection() {
    let radio = missing();
    let alive = Arc::new(AtomicUsize::new(0));
    let owned = alive.clone();
    {
        let mut state = radio.shared.lock().unwrap();
        state.generation = u64::MAX;
        state.opener = Box::new(move |_| {
            owned.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(TestRadio {
                online: Arc::new(AtomicBool::new(true)),
                ready: Arc::new(AtomicBool::new(true)),
                sent: Arc::new(Mutex::new(Vec::new())),
                alive: owned.clone(),
            }))
        });
    }
    assert!(radio.connect().is_none());
    assert_eq!(
        radio
            .connection_generation()
            .unwrap()
            .load(Ordering::Acquire),
        0
    );
    assert_eq!(radio.shared.lock().unwrap().generation, u64::MAX);
    assert_eq!(alive.load(Ordering::Acquire), 0);
}
struct CountedTransportSpy {
    base: TestRadio,
    calls: Arc<Mutex<Vec<&'static str>>>,
}
impl CommsDevice for CountedTransportSpy {
    fn recv_packet(&mut self, _: &Router, _: &mut dyn FnMut(&Packet)) -> TelemetryResult<()> {
        Ok(())
    }
    fn set_side_id(&mut self, _: RouterSideId) {}
    fn send_data(&mut self, data: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.base.send_data(data)
    }
    fn try_clone_tx(&self) -> std::io::Result<Option<Box<dyn CommsDevice>>> {
        Ok(Some(Box::new(Self {
            base: self.base.copy(),
            calls: self.calls.clone(),
        })))
    }
    fn enable_radio_capacity_control(&mut self) {
        self.calls.lock().unwrap().push("capacity");
    }
    fn send_tracked_counted(
        &mut self,
        _: &[(u8, &[u8])],
        _: &[sedsnet::router::PackedWriteLease],
    ) -> Result<Option<usize>, Box<dyn Error + Send + Sync>> {
        self.calls.lock().unwrap().push("counted");
        Ok(Some(7))
    }
    fn ordinary_background_state(&self) -> Option<crate::comms::OrdinaryLaneState> {
        Some(crate::comms::OrdinaryLaneState::Ready)
    }
    fn recovery_budget_configured(&self) -> bool {
        true
    }
    fn service_background_transmit_counted(
        &mut self,
        ordinary: bool,
    ) -> TelemetryResult<Option<crate::comms::BackgroundProgress>> {
        assert!(ordinary);
        self.calls.lock().unwrap().push("background");
        Ok(Some(crate::comms::BackgroundProgress {
            physical_completed: true,
            ordinary_charge: 13,
        }))
    }
    fn service_paid_budget_transmit_counted(
        &mut self,
    ) -> TelemetryResult<Option<crate::comms::BackgroundProgress>> {
        self.calls.lock().unwrap().push("paid");
        Ok(Some(crate::comms::BackgroundProgress {
            physical_completed: true,
            ordinary_charge: 17,
        }))
    }
}
#[test]
fn primary_wrapper_preserves_counted_background_and_capacity_contracts() {
    let config = missing().shared.lock().unwrap().config.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut radio = RecoveryRadio::from_open(
        config,
        Box::new(CountedTransportSpy {
            base: TestRadio {
                online: Arc::new(AtomicBool::new(true)),
                ready: Arc::new(AtomicBool::new(true)),
                sent: Arc::new(Mutex::new(Vec::new())),
                alive: Arc::new(AtomicUsize::new(1)),
            },
            calls: calls.clone(),
        }),
    )
    .unwrap();
    assert!(
        calls.lock().unwrap().is_empty(),
        "from_open must not introduce capacity changes or writes"
    );
    let mut tx = radio.try_clone_tx().unwrap().unwrap();
    assert_eq!(
        tx.send_tracked_counted(&[(255, &[1, 2])], &[]).unwrap(),
        Some(7)
    );
    assert_eq!(
        tx.ordinary_background_state(),
        Some(crate::comms::OrdinaryLaneState::Ready)
    );
    assert!(tx.recovery_budget_configured());
    let background = tx
        .service_background_transmit_counted(true)
        .unwrap()
        .unwrap();
    assert!(background.physical_completed);
    assert_eq!(background.ordinary_charge, 13);
    let paid = tx.service_paid_budget_transmit_counted().unwrap().unwrap();
    assert!(paid.physical_completed);
    assert_eq!(paid.ordinary_charge, 17);
    radio.enable_radio_capacity_control();
    assert!(radio.shared.lock().unwrap().capacity_enabled);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["counted", "background", "paid", "capacity"]
    );
}
