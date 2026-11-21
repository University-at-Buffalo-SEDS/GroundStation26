use super::*;
use crate::comms::{CommsDevice, MAX_ORDINARY_SERVICE_CHARGE, OrdinaryLaneState};

#[test]
fn completed_byte_balance_is_bounded_and_does_not_bank_while_peer_blocked() {
    let mut service = OrdinaryServiceBalance::default();
    assert_eq!(std::mem::size_of::<OrdinaryServiceBalance>(), 4);
    service.observe(OrdinaryLaneState::Ready);
    assert!(!service.allow_mesh(true));
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    assert!(service.allow_mesh(true));
    service.mesh_complete(108, true);
    service.mesh_complete(108, true);
    assert_eq!(service.bytes, 4);
    service.mesh_complete(108, true);
    assert_eq!(service.bytes, -104); // exactly one completed owner may cross zero.
    assert!(!service.allow_mesh(true));
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    let owed = service.bytes;
    service.observe(OrdinaryLaneState::Blocked);
    service.sdk_complete(220, OrdinaryLaneState::Blocked);
    service.mesh_complete(108, false); // SDK temporarily not ready; no credit bank.
    assert_eq!(service.bytes, owed);
    service.sdk_complete(0, OrdinaryLaneState::Ready);
    service.mesh_complete(0, true);
    assert_eq!(service.bytes, owed); // no successful ordinary bytes: no debit.
    service.sdk_complete(usize::MAX, OrdinaryLaneState::Ready);
    assert_eq!(service.bytes, MAX_ORDINARY_SERVICE_CHARGE as i32);
    service.mesh_complete(usize::MAX, true);
    assert_eq!(service.bytes, -(MAX_ORDINARY_SERVICE_CHARGE as i32));
    service.observe(OrdinaryLaneState::Empty);
    assert_eq!(service.bytes, 0);
    service.sdk_complete(220, OrdinaryLaneState::Ready);
    service.observe(OrdinaryLaneState::Ready);
    assert_eq!(
        service.bytes, 220,
        "a ready transport with a gap before the next SDK cursor offer must not erase earned Mesh history"
    );
}

#[derive(Default)]
struct GenericCountedDevice {
    calls: usize,
    bytes: usize,
    fail: bool,
}
impl CommsDevice for GenericCountedDevice {
    fn recv_packet(
        &mut self,
        _: &Router,
        _: &mut dyn FnMut(&sedsnet::packet::Packet),
    ) -> sedsnet::TelemetryResult<()> {
        Ok(())
    }
    fn send_data(
        &mut self,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.calls += 1;
        if self.fail {
            return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into());
        }
        self.bytes += payload.len();
        Ok(())
    }
    fn set_side_id(&mut self, _: sedsnet::router::RouterSideId) {}
}

#[test]
fn unsupported_counted_send_has_already_sent_exactly_once_and_preserves_errors() {
    let mut device = GenericCountedDevice::default();
    assert_eq!(device.ordinary_background_state(), None);
    assert_eq!(
        device.send_tracked_counted(&[(0, b"opaque")], &[]).unwrap(),
        None
    );
    assert_eq!((device.calls, device.bytes), (1, 6));
    device.fail = true;
    assert!(
        device
            .send_tracked_counted(&[(0, b"refused")], &[])
            .is_err()
    );
    assert_eq!((device.calls, device.bytes), (2, 6));
}

// Force the exact readiness crossing without sleeps or a wall/virtual clock race.
// The real worker first earns an ordinary turn, then the counted Mesh attempt
// returns no progress while making the next transmit_ready() read true.
struct ReadyEdgeDevice {
    phase: usize,
    after_refusal: Option<OrdinaryLaneState>,
    trace: Arc<Mutex<Vec<&'static str>>>,
    sender: mpsc::UnboundedSender<(u8, Vec<u8>)>,
    budget: Arc<TxQueueBudget>,
    command: Vec<u8>,
}
impl CommsDevice for ReadyEdgeDevice {
    fn recv_packet(
        &mut self,
        _: &Router,
        _: &mut dyn FnMut(&Packet),
    ) -> sedsnet::TelemetryResult<()> {
        Ok(())
    }
    fn set_side_id(&mut self, _: sedsnet::router::RouterSideId) {}
    fn transmit_ready(&self) -> bool {
        self.phase != 1
    }
    fn ordinary_background_state(&self) -> Option<OrdinaryLaneState> {
        match self.phase {
            0 | 1 => Some(OrdinaryLaneState::Ready),
            2 => self.after_refusal,
            _ => Some(OrdinaryLaneState::Empty),
        }
    }
    fn service_background_transmit_counted(
        &mut self,
        allow: bool,
    ) -> sedsnet::TelemetryResult<Option<crate::comms::BackgroundProgress>> {
        if allow && self.phase == 1 {
            assert!(!self.transmit_ready());
            self.phase = 2;
            assert!(self.transmit_ready());
            if self.after_refusal == Some(OrdinaryLaneState::Ready) {
                self.budget
                    .try_send(&self.sender, 255, &self.command)
                    .unwrap();
            }
            return Ok(Some(crate::comms::BackgroundProgress::default()));
        }
        if allow && self.phase == 2 && self.after_refusal == Some(OrdinaryLaneState::Ready) {
            self.trace.lock().unwrap().push("mesh");
            self.phase = 3;
            return Ok(Some(crate::comms::BackgroundProgress {
                physical_completed: true,
                ordinary_charge: 108,
            }));
        }
        Ok(Some(crate::comms::BackgroundProgress::default()))
    }
    fn send_data(&mut self, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let label = if bytes == b"ordinary-first" {
            assert_eq!(self.phase, 0);
            self.phase = 1;
            "sdk-first"
        } else if bytes == b"ordinary-second" {
            self.phase = 4;
            "sdk-second"
        } else {
            assert_eq!(bytes, self.command);
            "critical"
        };
        self.trace.lock().unwrap().push(label);
        Ok(())
    }
    fn send_tracked_counted(
        &mut self,
        frames: &[(u8, &[u8])],
        leases: &[sedsnet::router::PackedWriteLease],
    ) -> Result<Option<usize>, Box<dyn std::error::Error + Send + Sync>> {
        assert!(leases.is_empty());
        assert_eq!(frames.len(), 1);
        self.send_data(frames[0].1)?;
        Ok(Some(if frames[0].0 == 255 { 76 } else { 220 }))
    }
}

#[tokio::test]
async fn owed_mesh_turn_survives_readiness_edge_without_blocking_critical_or_blocked_peer() {
    crate::telemetry_schema::initialize().unwrap();
    for after_refusal in [
        Some(OrdinaryLaneState::Ready),
        Some(OrdinaryLaneState::Blocked),
        Some(OrdinaryLaneState::Empty),
        None,
    ] {
        let state = crate::state::tests::test_app_state().await;
        let router = Arc::new(Router::new(Default::default()));
        let command = serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
                &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")],
                "GS",
                30000,
                Arc::from([1u8]),
            )
            .unwrap(),
        )
        .to_vec();
        let (sender, receiver) = mpsc::unbounded_channel();
        let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
        budget.try_send(&sender, 0, b"ordinary-first").unwrap();
        budget.try_send(&sender, 0, b"ordinary-second").unwrap();
        let trace = Arc::new(Mutex::new(Vec::new()));
        let device = ReadyEdgeDevice {
            phase: 0,
            after_refusal,
            trace: trace.clone(),
            sender: sender.clone(),
            budget: budget.clone(),
            command,
        };
        let workers = spawn_comms_worker_threads(
            router,
            state.clone(),
            CommsWorkerHandle {
                transport_activity: None,
                name: "rocket_comms",
                tx_budget: Some(budget.clone()),
                comms: Arc::new(Mutex::new(Box::new(GenericCountedDevice::default()))),
                tx_comms: Some(Arc::new(Mutex::new(Box::new(device)))),
                side_id: 0,
                tx_rx: receiver,
                legacy_single_worker: false,
                prioritize_rx: false,
                dedicated_radio_io: false,
            },
        )
        .unwrap();
        let finished = tokio::time::timeout(Duration::from_secs(2), async {
            while !trace.lock().unwrap().contains(&"sdk-second") {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        state.request_shutdown();
        drop(sender);
        for worker in workers {
            worker.join().unwrap();
        }
        finished
            .expect("retained turn must progress; blocked/empty/unknown peer cannot deadlock SDK");
        let expected = if after_refusal == Some(OrdinaryLaneState::Ready) {
            vec!["sdk-first", "critical", "mesh", "sdk-second"]
        } else {
            vec!["sdk-first", "sdk-second"]
        };
        assert_eq!(
            *trace.lock().unwrap(),
            expected,
            "post-attempt Mesh state {after_refusal:?}"
        );
        assert_eq!(
            budget.pending.load(Ordering::Acquire),
            0,
            "every accepted owner remains until actual send"
        );
    }
}
