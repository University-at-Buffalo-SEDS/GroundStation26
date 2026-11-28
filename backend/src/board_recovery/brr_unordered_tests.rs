//! BOARD_RECOVERY_REQUEST was one Ordered SDK stream for every board: a
//! request to one board waited until the one ahead of it, to any board, was
//! acknowledged, and was cancelled if that one ran out of retries.
//! - pair-dev / pair-all / pair-reset: a RECOVER for PB cancelled behind an
//!   exhausted request to RF (Reset all failed);
//! - sched-winner: FC's Abort readback query held about 8 s behind an AV-bay
//!   background query at the head of the stream.
//! Nothing on a board or in the station's exchange depends on the SDK's order
//! between two of these requests (each exchange step waits for the board's
//! reply first; boards check identity, freshness, epoch and armed QUERY), so
//! the type is Unordered. Real SDK routers, station delivery tuning.
use super::*;
use sedsnet::router::{EndpointHandler, Router, RouterConfig};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn request(router: &Router, board: Board, nonce: u64) -> Packet {
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

/// The station and two boards, each on its own side. `lossy` loses every
/// frame back from the first board (its receipts). Returns the ms after the
/// second request was queued at which the second board received it (None:
/// not within `run`), and how many requests the first board received.
fn second_request_delay(first: Board, second: Board, run: Duration) -> (Option<u128>, usize) {
    crate::telemetry_schema::initialize().unwrap();
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, crate::get_current_timestamp_ms());
    let counter = |board: Board, got: Arc<AtomicUsize>| Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint(endpoint(board)),
            move |_| { got.fetch_add(1, Ordering::SeqCst); Ok(()) },
        )]).with_sender(board.sender_id())));
    let (a_got, b_got) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (a, b) = (counter(first, a_got.clone()), counter(second, b_got.clone()));
    let wires: Vec<Arc<std::sync::Mutex<Vec<Vec<u8>>>>> = (0..4).map(|_| Arc::default()).collect();
    let tap = |w: &Arc<std::sync::Mutex<Vec<Vec<u8>>>>| { let w = w.clone(); move |bytes: &[u8]| { w.lock().unwrap().push(bytes.to_vec()); Ok(()) } };
    host.add_side_packed("first", tap(&wires[0]));
    a.add_side_packed("up", tap(&wires[1]));
    host.add_side_packed("second", tap(&wires[2]));
    b.add_side_packed("up", tap(&wires[3]));
    let lossy = AtomicBool::new(false);
    let pump = || {
        let _ = host.process_all_queues_with_timeout(0);
        for f in std::mem::take(&mut *wires[0].lock().unwrap()) { let _ = a.rx_packed_from_side(&f, 0); }
        for f in std::mem::take(&mut *wires[2].lock().unwrap()) { let _ = b.rx_packed_from_side(&f, 0); }
        let _ = a.process_all_queues_with_timeout(0);
        let _ = b.process_all_queues_with_timeout(0);
        for f in std::mem::take(&mut *wires[1].lock().unwrap()) { if !lossy.load(Ordering::SeqCst) { let _ = host.rx_packed_from_side(&f, 0); } }
        for f in std::mem::take(&mut *wires[3].lock().unwrap()) { let _ = host.rx_packed_from_side(&f, 1); }
    };
    a.announce_discovery().unwrap();
    b.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 { pump(); std::thread::sleep(Duration::from_millis(1)); }
    lossy.store(true, Ordering::SeqCst);
    host.tx_queue(request(&host, first, 1)).unwrap();
    for _ in 0..10 { pump(); std::thread::sleep(Duration::from_millis(2)); }
    let start = WallInstant::now();
    host.tx_queue(request(&host, second, 2)).unwrap();
    let mut at = None;
    while start.elapsed() < run && at.is_none() {
        pump();
        if b_got.load(Ordering::SeqCst) > 0 { at = Some(start.elapsed().as_millis()); }
        std::thread::sleep(Duration::from_millis(2));
    }
    (at, a_got.load(Ordering::SeqCst))
}

/// A RECOVER for the Power Board while a request to the RF Board, its
/// receipts lost, is still being retried (it would run out of retries): the
/// PB request goes at once, not after (or cancelled with) the RF one.
#[test]
fn a_pb_recover_is_not_held_or_cancelled_behind_an_exhausting_rf_request() {
    let _tuning = crate::network_delivery_tuning_for_test();
    let (at, rf) = second_request_delay(Board::RFBoard, Board::PowerBoard, Duration::from_secs(8));
    eprintln!("BRR_UNORDERED pb_at={at:?} rf_got={rf}");
    assert!(rf >= 1, "the RF Board got its request; only its receipts were lost");
    let at = at.expect("the PB request reached the Power Board");
    assert!(at < 1000, "PB request delivered after {at} ms");
}

/// sched-winner: FC's Abort readback query behind an AV-bay background
/// query still in flight. The FC query goes at once.
#[test]
fn an_fc_readback_query_is_not_held_behind_an_in_flight_background_query() {
    let _tuning = crate::network_delivery_tuning_for_test();
    let (at, rf) = second_request_delay(Board::RFBoard, Board::FlightComputer, Duration::from_secs(8));
    eprintln!("BRR_UNORDERED fc_at={at:?} rf_got={rf}");
    assert!(rf >= 1);
    let at = at.expect("the FC query reached the FC");
    assert!(at < 1000, "FC query delivered after {at} ms");
}

/// The station's schema and every board's carry the same definition.
#[test]
fn board_recovery_request_is_unordered_in_the_station_schema() {
    crate::telemetry_schema::initialize().unwrap();
    let config: serde_json::Value = serde_json::from_str(include_str!("../../config/telemetry_config.json")).unwrap();
    let brr = config["types"].as_array().unwrap().iter().find(|t| t["name"] == QUERY_TYPE).unwrap();
    assert_eq!(brr["reliable"], "Unordered");
}
