#[derive(Clone)]
struct PendingEpochReceipt {
    state: usize,
    packet: u64,
    seen: Arc<std::sync::atomic::AtomicBool>,
}
static PENDING_EPOCH_RECEIPTS: std::sync::OnceLock<Mutex<Vec<PendingEpochReceipt>>> =
    std::sync::OnceLock::new();
fn pending_epoch_note_ingress(state: &AppState, packet: &Packet) {
    let Some(watches) = PENDING_EPOCH_RECEIPTS.get() else {
        return;
    };
    let seen = watches
        .lock()
        .unwrap()
        .iter()
        .find(|watch| {
            watch.state == state as *const _ as usize && watch.packet == packet.packet_id()
        })
        .map(|watch| watch.seen.clone());
    if let Some(seen) = seen {
        seen.store(true, Ordering::Release);
    }
}
struct PendingEpochWatch(PendingEpochReceipt);
impl PendingEpochWatch {
    fn install(state: &AppState, packet: &Packet) -> Self {
        let watch = PendingEpochReceipt {
            state: state as *const _ as usize,
            packet: packet.packet_id(),
            seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        PENDING_EPOCH_RECEIPTS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .push(watch.clone());
        Self(watch)
    }
}
impl Drop for PendingEpochWatch {
    fn drop(&mut self) {
        if let Some(watches) = PENDING_EPOCH_RECEIPTS.get() {
            watches
                .lock()
                .unwrap()
                .retain(|watch| watch.state != self.0.state || watch.packet != self.0.packet);
        }
    }
}
// Synthetic board responses over the actual peer Router/SDK and HTTP coordinator.
// This checks protocol ownership, not MCU application or physical outputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_next_epoch_actual_sdk_requires_terminal_and_final_query() {
    if crate::operator_queue::isolated_native_case(
        module_path!(),
        "pending_next_epoch_actual_sdk_requires_terminal_and_final_query",
    ) {
        return;
    }
    for ending in ["terminal", "timeout", "manual_stop"] {
        let fixture = Fixture::new().await;
        let state = fixture.state();
        *state.state.lock().unwrap() = crate::types::FlightState::Aborted;
        let model = fixture
            .models
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .clone();
        model.lock().unwrap().pending_only = true;
        fixture.query(Board::ActuatorBoard).await;
        let ticket = fixture.prepare("recover_abort", Some("AB")).await;
        assert_eq!(fixture.confirm(&ticket).await.status(), StatusCode::OK);
        let original = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let original = model
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .find(|p| p.payload()[1] == 1)
                    .cloned();
                if let Some(original) = original {
                    break original;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let request = original.payload();
        let mut payload = [0u8; 40];
        payload[0] = 1;
        payload[1] = 1;
        payload[2] = request[2];
        payload[3] = 0x7e;
        payload[4..12].copy_from_slice(&request[4..12]);
        payload[12..20].copy_from_slice(&request[12..20]);
        let epoch = u32::from_le_bytes(request[20..24].try_into().unwrap());
        payload[20..24].copy_from_slice(&epoch.checked_add(1).unwrap().to_le_bytes());
        payload[28..32].copy_from_slice(
            &safe_output_mask(Board::ActuatorBoard)
                .unwrap()
                .to_le_bytes(),
        );
        payload[32..40].copy_from_slice(&request[24..32]);
        let peer = fixture
            .native_peers
            .iter()
            .find(|(b, _)| *b == Board::ActuatorBoard)
            .unwrap()
            .1
            .clone();
        let progress = Packet::new(
            crate::telemetry_schema::data_type(STATUS_TYPE),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            &peer.sender(),
            original.timestamp(),
            Arc::from(payload),
        )
        .unwrap();
        let receipt = PendingEpochWatch::install(&state, &progress);
        peer.tx_queue(progress).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !receipt.0.seen.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the exact next-epoch Pending must traverse actual host SDK ingress");
        let polls_after_actual_receipt = model
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|p| p.payload()[1] == 1)
            .count();
        // The real owner must survive this exact progress reply and issue a
        // subsequent immutable poll. Old code instead terminates with epoch error.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let completed = state
                    .board_recovery
                    .coordinator
                    .operation()
                    .is_some_and(|op| op["completed"] == true);
                let polls = model
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|p| p.payload()[1] == 1)
                    .count();
                if completed || polls > polls_after_actual_receipt {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert_ne!(
            state.board_recovery.coordinator.operation().unwrap()["completed"],
            true,
            "next-epoch Pending prematurely terminated actual coordinator: {ending}"
        );
        assert_eq!(
            model.lock().unwrap().applied,
            0,
            "Pending is never an application completion"
        );
        let queries_before = model
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|p| p.payload()[1] == 0)
            .count();
        if ending == "terminal" {
            model.lock().unwrap().pending_only = false;
        }
        if ending == "manual_stop" {
            state.manual_abort_intent.request();
        }
        let operation = fixture.terminal().await;
        assert_eq!(
            operation["success"],
            ending == "terminal",
            "{ending}: {operation}"
        );
        assert_eq!(
            state.local_flight_state_snapshot(),
            crate::types::FlightState::Aborted,
            "targeted recovery cannot clear global Abort"
        );
        let model = model.lock().unwrap();
        if ending == "terminal" {
            assert_eq!(model.applied, 1);
            assert!(
                model
                    .requests
                    .iter()
                    .filter(|p| p.payload()[1] == 0)
                    .count()
                    > queries_before,
                "actual fresh final QUERY required after terminal"
            );
        } else {
            assert_eq!(model.applied, 0);
        }
        for packet in model.requests.iter().filter(|p| p.payload()[1] == 1) {
            assert_eq!(packet.payload(), original.payload());
            assert_eq!(packet.timestamp(), original.timestamp());
        }
    }
}
