use super::*;
use sedsnet::{
    packet::Packet,
    router::{Router, RouterConfig},
    wire_format,
};
fn retained_bulk(
    state: &Arc<AppState>,
    packed: bool,
) -> (Arc<Router>, u64, sedsnet::config::DataType) {
    let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    state.topology_router.set(router.clone()).unwrap();
    assert!(!manual_stop_pending(&state));
    assert!(!controller_delivery_pending(&state));
    let kind = crate::telemetry_schema::data_type("GENERIC_ERROR");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = seen.clone();
    let side = router.add_side_packed("blocked-bulk-only", move |bytes| {
        if let Ok(packet) = wire_format::unpack_packet(bytes) {
            capture
                .lock()
                .unwrap()
                .push((packet.packet_id(), packet.data_type().as_u32()));
        }
        if wire_format::peek_frame_info(bytes).is_ok_and(|info| info.envelope.ty == kind) {
            return Err(sedsnet::TelemetryError::Io("diagnostic busy"));
        }
        Ok(())
    });
    router
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "DAQ",
                123,
                &[crate::telemetry_schema::endpoint("SD_CARD")],
            )
            .unwrap(),
            side,
        )
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    assert!(!manual_stop_pending(&state));
    let bulk = Packet::new_with_nonce(
        kind,
        &[crate::telemetry_schema::endpoint("SD_CARD")],
        "GS",
        100,
        77,
        Arc::from(vec![0x53; 1024]),
    )
    .unwrap();
    let id = bulk.packet_id();
    if packed {
        router
            .tx_packed_queue(wire_format::pack_packet(&bulk).into())
            .unwrap();
    } else {
        router.tx_queue(bulk).unwrap();
    }
    assert!(
        !manual_stop_pending(state),
        "valid initial typed queue does not falsely claim Stop"
    );
    let processed = router.process_all_queues_with_timeout(0);
    let owners = router.export_reliable_pending_stats();
    assert_eq!(owners.len(), 1);
    assert_eq!((owners[0].packet_id, owners[0].data_type), (id, kind));
    assert_eq!(
        owners[0].pending_destinations,
        vec![sedsnet::packet::hash_bytes_u64(
            0x517C_C1B7_2722_0A95,
            b"DAQ"
        )]
    );
    assert_eq!(
        router.queue_counters()[2],
        0,
        "only the exact retained Packed E2E owner remains"
    );
    assert!(seen.lock().unwrap().contains(&(id, kind.as_u32())));
    eprintln!(
        "EXACT_RETAINED_BULK: id={id} type={} owners={owners:?} process={processed:?}",
        kind.as_u32()
    );
    (router, id, kind)
}

#[tokio::test]
async fn unrelated_packed_bulk_must_not_own_manual_stop_or_controller_delivery() {
    let state = crate::state::tests::test_app_state().await;
    let (router, id, kind) = retained_bulk(&state, true);
    let logical = router.export_reliable_pending_stats();
    let counters = router.queue_counters();
    let stop = manual_stop_pending(&state);
    let delivery = controller_delivery_pending(&state);
    let command = queue_controller_with_stop_fence(
        &state,
        &router,
        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
        &[ActuatorBoardCommands::NitrogenOpen as u8],
    )
    .unwrap();
    let launch = inhibit_automatic_actions_for_launch(&state).err();
    eprintln!(
        "PACKED_CAUSAL: bulk_id={id} bulk_type={} owners={logical:?} counters={counters:?} manual_stop={stop} controller_pending={delivery} admitted_open={command} launch={launch:?}",
        kind.as_u32()
    );
    assert!(logical.iter().all(|owner| owner.data_type == kind));
    assert!(
        !stop,
        "unrelated Packed bulk with zero ABORT owners must not assert manual Stop"
    );
}

#[tokio::test]
async fn genuine_abort_queued_claimed_retrying_and_exact_receipt_control() {
    use sedsnet::router::{EndpointHandler, RouterSideOptions};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    let state = crate::state::tests::test_app_state().await;
    let now = Arc::new(AtomicU64::new(1000));
    let clock = now.clone();
    let router = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender("GS"),
        Box::new(move || clock.load(Ordering::SeqCst)),
    ));
    state.topology_router.set(router.clone()).unwrap();
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = delivered.clone();
    let ep = crate::telemetry_schema::endpoint("ABORT");
    let kind = crate::telemetry_schema::data_type("ABORT");
    let clock = now.clone();
    let peer = Arc::new(Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(ep, move |packet| {
            capture
                .lock()
                .unwrap()
                .push((packet.packet_id(), packet.data_type().as_u32()));
            Ok(())
        })])
        .with_sender("DAQ"),
        Box::new(move || clock.load(Ordering::SeqCst)),
    ));
    let host_side = Arc::new(std::sync::OnceLock::new());
    let ingress = host_side.clone();
    let host = Arc::downgrade(&router);
    let options = RouterSideOptions {
        reliable_enabled: false,
        link_local_enabled: true,
        ..Default::default()
    };
    let peer_side = peer.add_side_packed_with_options(
        "to-ground",
        move |wire| {
            host.upgrade()
                .unwrap()
                .rx_packed_from_side(wire, *ingress.get().unwrap())
        },
        options.clone(),
    );
    let blocked = Arc::new(AtomicBool::new(true));
    let busy = blocked.clone();
    let app = Arc::downgrade(&state);
    let receiver = peer.clone();
    let claimed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = claimed.clone();
    let side = router.add_side_packed_with_options(
        "to-daq",
        move |wire| {
            if wire_format::peek_frame_info(wire).is_ok_and(|info| info.envelope.ty == kind) {
                let envelope = wire_format::peek_envelope(wire).unwrap();
                capture.lock().unwrap().push((
                    wire_format::packet_id_from_wire(wire).unwrap(),
                    envelope.target_senders.to_vec(),
                    manual_stop_pending(&app.upgrade().unwrap()),
                ));
                if busy.load(Ordering::SeqCst) {
                    return Err(sedsnet::TelemetryError::Io("ABORT control busy"));
                }
            }
            receiver.rx_packed_from_side(wire, peer_side)
        },
        options,
    );
    host_side.set(side).unwrap();
    peer.announce_discovery().unwrap();
    for _ in 0..10 {
        peer.process_all_queues_with_timeout(0).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
    }
    assert!(!manual_stop_pending(&state));
    let packet = Packet::new_with_nonce(
        kind,
        &[ep],
        "GS",
        100,
        999,
        Arc::from(&b"diagnostic genuine manual Stop"[..]),
    )
    .unwrap();
    let id = packet.packet_id();
    let owner = sedsnet::packet::hash_bytes_u64(0x517C_C1B7_2722_0A95, b"DAQ");
    router.tx_queue(packet).unwrap();
    let queued = router.queue_counters();
    assert_eq!(queued[2], 1);
    assert_eq!(queued[5], kind.as_u32());
    assert!(manual_stop_pending(&state));
    let _ = router.process_all_queues_with_timeout(0);
    let owners = router.export_reliable_pending_stats();
    assert_eq!(owners.len(), 1);
    assert_eq!((owners[0].packet_id, owners[0].data_type), (id, kind));
    assert_eq!(owners[0].pending_destinations, vec![owner]);
    assert!(
        claimed
            .lock()
            .unwrap()
            .iter()
            .all(|(found, target, pending)| *found == id && target == &vec![owner] && *pending)
    );
    assert!(!claimed.lock().unwrap().is_empty());
    assert!(manual_stop_pending(&state));
    now.store(2000, Ordering::SeqCst);
    let _ = router.periodic(0);
    let _ = router.process_all_queues_with_timeout(0);
    let retry = router.export_reliable_pending_stats();
    assert_eq!(retry.len(), 1);
    assert_eq!((retry[0].packet_id, retry[0].data_type), (id, kind));
    assert_eq!(retry[0].pending_destinations, vec![owner]);
    assert!(
        retry[0].retries > 0,
        "actual retry ownership must be exercised"
    );
    assert!(manual_stop_pending(&state));
    assert!(delivered.lock().unwrap().is_empty());
    blocked.store(false, Ordering::SeqCst);
    now.store(3000, Ordering::SeqCst);
    let _ = router.periodic(0);
    for _ in 0..30 {
        let _ = router.process_all_queues_with_timeout(0);
        peer.process_all_queues_with_timeout(0).unwrap();
    }
    assert_eq!(*delivered.lock().unwrap(), vec![(id, kind.as_u32())]);
    assert!(
        router
            .export_reliable_pending_stats()
            .iter()
            .all(|row| row.packet_id != id)
    );
    assert!(!manual_stop_pending(&state));
    assert!(!controller_delivery_pending(&state));
    eprintln!(
        "GENUINE_ABORT_CONTROL: exact_id={id} exact_owner={owner} queued_type={} claimed={:?} retry={retry:?} delivered={:?} retired_stop={}",
        queued[5],
        *claimed.lock().unwrap(),
        *delivered.lock().unwrap(),
        manual_stop_pending(&state)
    );
}

#[tokio::test]
async fn unrelated_packed_bulk_must_not_block_authenticated_daq_retry() {
    use crate::recording_export::tests::{TestDirectory, export_state};
    use axum::http::{HeaderMap, StatusCode};
    use tower::ServiceExt;
    for blocked in [false, true] {
        let root = TestDirectory::new();
        let state = export_state(&root.0, true).await;
        std::fs::write(root.0.join("auth.json"),serde_json::json!({
            "version":1,"anonymous":{"view_data":true,"send_commands":false},
            "users":[{"username":"sd-operator","password":{"iterations":120000,"salt_b64":"","hash_b64":""},
            "permissions":{"view_data":true,"send_commands":true},"command_access":{"allowed_commands":["DaqSdRetry"]}}]
        }).to_string()).unwrap();
        let now = crate::telemetry_task::get_current_timestamp_ms() as i64;
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('sd-test','sd-operator','session',1,1,?,?)")
            .bind(now).bind(now+60000).execute(&state.auth_db).await.unwrap();
        let router = if blocked {
            retained_bulk(&state, true).0
        } else {
            let router = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
            state.topology_router.set(router.clone()).unwrap();
            router
        };
        state.network_health.lock().unwrap().observe_sd_status(
            "DAQ",
            &[1, u32::MAX, u32::MAX, 1, 0, 0, 0, 0],
            std::time::Instant::now(),
            now as u64,
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer sd-test".parse().unwrap(),
        );
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/api/network-health/daq/sd/retry")
            .body(axum::body::Body::empty())
            .unwrap();
        *request.headers_mut() = headers;
        let response = crate::web::router(state, String::new())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        eprintln!(
            "ACTUAL_AUTHENTICATED_DAQ_RETRY blocked_bulk={blocked}: status={status} body={} exact_owners={:?}",
            String::from_utf8_lossy(&body),
            router.export_reliable_pending_stats()
        );
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "unrelated exact bulk ownership must not be mistaken for an already-pending DAQ retry"
        );
    }
}

#[tokio::test]
async fn decoded_bulk_packed_by_sdk_must_not_assert_stop() {
    let state = crate::state::tests::test_app_state().await;
    let (router, id, kind) = retained_bulk(&state, false);
    assert!(!manual_stop_pending(&state));
    assert!(!controller_delivery_pending(&state));
    assert!(
        queue_controller_with_stop_fence(
            &state,
            &router,
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[ActuatorBoardCommands::NitrogenOpen as u8]
        )
        .unwrap()
    );
    eprintln!(
        "DECODED_REPRESENTATION_CONTROL: same_exact_id={id} same_type={} same_DAQ_owner; actual_open_admitted=true, stop=false before/after; retained unrelated Packet correctly classified",
        kind.as_u32()
    );
    assert!(!manual_stop_pending(&state));
}

#[cfg(feature = "hitl_mode")]
#[tokio::test]
async fn unrelated_retained_data_does_not_block_launch_admission() {
    let state = crate::state::tests::test_app_state().await;
    let (router, id, kind) = retained_bulk(&state, true);
    assert!(
        router
            .export_reliable_pending_stats()
            .iter()
            .all(|row| row.packet_id == id && row.data_type == kind)
    );
    assert!(!controller_delivery_pending(&state));
    assert!(inhibit_automatic_actions_for_launch(&state).is_ok());
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
}
