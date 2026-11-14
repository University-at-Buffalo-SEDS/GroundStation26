//! Reset all, pair-dev / pair-all (GS 0ea3166, 50% RF->GS loss): every failed
//! Reset all fell in a window where a BOARD_RECOVERY_REQUEST to an AV-bay
//! board ran out of retries. BOARD_RECOVERY_REQUEST is one Ordered SDK stream,
//! so the SDK then cancelled every query queued behind it: the umbilical
//! boards' PREPARE queries ("Fresh query timed out before challenge"), the
//! other AV-bay queries and the proving readbacks. The coordinator waited out
//! its whole window for replies that could not come. Real SDK routers here,
//! with the station's delivery tuning; the board models only count queries.
use super::*;
use sedsnet::router::{EndpointHandler, Router, RouterConfig};

fn query_packet(router: &Router, board: Board, nonce: u64) -> Packet {
    let mut wire = [0u8; 32];
    wire[0] = 1;
    wire[4..12].copy_from_slice(&nonce.to_le_bytes());
    Packet::new(
        crate::telemetry_schema::data_type(QUERY_TYPE),
        &[crate::telemetry_schema::endpoint(endpoint(board))],
        &router.sender(),
        crate::get_current_timestamp_ms(),
        Arc::from(wire),
    )
    .unwrap()
}

/// A board router that counts the recovery queries it is delivered.
fn board_router(board: Board, got: Arc<std::sync::atomic::AtomicUsize>) -> Arc<Router> {
    Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint(endpoint(board)),
            move |_packet| {
                got.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )])
        .with_sender(board.sender_id()),
    ))
}

#[tokio::test]
async fn a_query_cancelled_behind_an_exhausted_radio_query_is_asked_again() {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let _tuning = crate::network_delivery_tuning_for_test();
    crate::telemetry_schema::initialize().unwrap();
    let state = crate::state::tests::test_app_state().await;
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, crate::get_current_timestamp_ms());
    let (fc_got, vb_got) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let fc = board_router(Board::FlightComputer, fc_got.clone());
    let vb = board_router(Board::ValveBoard, vb_got.clone());
    // The radio carries every frame to the FC; while lossy, none comes back
    // (its receipts are lost). The umbilical works both ways.
    let lossy = Arc::new(AtomicBool::new(false));
    let wires: Vec<Arc<std::sync::Mutex<Vec<Vec<u8>>>>> = (0..4).map(|_| Arc::default()).collect();
    let tap = |w: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>| { let w = w.clone(); move |b: &[u8]| { w.lock().unwrap().push(b.to_vec()); Ok(()) } };
    host.add_side_packed("radio", tap(&wires[0]));
    fc.add_side_packed("radio", tap(&wires[1]));
    host.add_side_packed("umbilical", tap(&wires[2]));
    vb.add_side_packed("umbilical", tap(&wires[3]));
    let pump = || {
        let _ = host.process_all_queues_with_timeout(0);
        for b in std::mem::take(&mut *wires[0].lock().unwrap()) { let _ = fc.rx_packed_from_side(&b, 0); }
        for b in std::mem::take(&mut *wires[2].lock().unwrap()) { let _ = vb.rx_packed_from_side(&b, 0); }
        let _ = fc.process_all_queues_with_timeout(0);
        let _ = vb.process_all_queues_with_timeout(0);
        for b in std::mem::take(&mut *wires[1].lock().unwrap()) { if !lossy.load(Ordering::SeqCst) { let _ = host.rx_packed_from_side(&b, 0); } }
        for b in std::mem::take(&mut *wires[3].lock().unwrap()) { let _ = host.rx_packed_from_side(&b, 1); }
    };
    fc.announce_discovery().unwrap();
    vb.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 { pump(); std::thread::sleep(Duration::from_millis(1)); }
    assert!(state.topology_router.set(host.clone()).is_ok());
    lossy.store(true, Ordering::SeqCst);

    // An AV-bay query at the head of the one Ordered stream, then a PREPARE
    // query to the umbilical Valve Board behind it.
    host.tx_queue(query_packet(&host, Board::FlightComputer, 11)).unwrap();
    let vb_query = query_packet(&host, Board::ValveBoard, 22);
    super::super::note_query_packet(&state, Board::ValveBoard, 22, vb_query.packet_id());
    let mut watch = QueryWatch::new();
    host.tx_queue(vb_query).unwrap();
    let start = WallInstant::now();
    let mut dropped_at = None;
    while start.elapsed() < Duration::from_secs(15) && dropped_at.is_none() {
        pump();
        if watch.dropped(&state, Board::ValveBoard, 22) { dropped_at = Some(start.elapsed()); }
        std::thread::sleep(Duration::from_millis(5));
    }
    let span = {
        let t = sedsnet::config::runtime_tuning_config();
        Duration::from_millis(u64::from(t.reliable_retransmit_ms) * u64::from(t.reliable_max_retries + 1))
    };
    eprintln!("QUERY_REASK fc_got={} vb_got={} dropped_at={dropped_at:?} span={span:?}",
              fc_got.load(Ordering::SeqCst), vb_got.load(Ordering::SeqCst));
    // The bug: the FC query ran out of retries and the SDK cancelled the
    // Valve Board's query behind it; the board never got it.
    assert!(fc_got.load(Ordering::SeqCst) >= 1, "the FC got its query; only its receipts were lost");
    assert_eq!(vb_got.load(Ordering::SeqCst), 0, "cancelled behind the exhausted FC query");
    // The fix: the drop is seen soon after, well inside the 15 s window, and
    // the query asked again reaches the board.
    let at = dropped_at.expect("the cancelled query is seen dropped");
    assert!(at < span + network_wait(QUERY_REPLY_GRACE) + Duration::from_secs(2), "seen dropped after {at:?}");
    host.tx_queue(query_packet(&host, Board::ValveBoard, 23)).unwrap();
    for _ in 0..50 { pump(); std::thread::sleep(Duration::from_millis(2)); }
    assert_eq!(vb_got.load(Ordering::SeqCst), 1, "the query asked again reaches the Valve Board");
}

#[test]
fn a_query_is_dropped_only_after_it_leaves_the_sdk_and_its_reply_grace() {
    let t0 = WallInstant::now();
    // Pending: never dropped, however long.
    let mut w = QueryWatch { sent: t0, checked: None, seen: false, gone: None };
    assert!(!query_dropped(&mut w, true, t0 + Duration::from_secs(60)));
    // Gone after being pending: the reply gets its grace.
    let gone = t0 + Duration::from_secs(1);
    assert!(!query_dropped(&mut w, false, gone));
    assert!(!query_dropped(&mut w, false, gone + network_wait(QUERY_REPLY_GRACE) - Duration::from_millis(1)));
    assert!(query_dropped(&mut w, false, gone + network_wait(QUERY_REPLY_GRACE)));
    // Seen pending again (a retransmit): the grace starts over.
    assert!(!query_dropped(&mut w, true, gone + Duration::from_secs(5)));
    assert!(!query_dropped(&mut w, false, gone + Duration::from_secs(5)));
    // Never seen pending: it may still wait in the TX queue for QUERY_UNSEEN.
    let mut u = QueryWatch { sent: t0, checked: None, seen: false, gone: None };
    assert!(!query_dropped(&mut u, false, t0 + network_wait(QUERY_UNSEEN) - Duration::from_millis(1)));
    let late = t0 + network_wait(QUERY_UNSEEN);
    assert!(!query_dropped(&mut u, false, late));
    assert!(query_dropped(&mut u, false, late + network_wait(QUERY_REPLY_GRACE)));
}
