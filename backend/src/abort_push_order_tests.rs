//! An AV-bay board's Abort push must not wait behind an earlier status whose
//! ACK has not come back. In a 0%-loss fullnet run the FC's previous
//! BOARD_STATE_STATUS (a query reply) lost one end-to-end ACK; its Abort push,
//! queued 150 ms after the Abort, left the radio only when that reply's second
//! ACK arrived, 1.2 s after the press. The FC's IMU and barometer frames kept
//! flowing the whole time, so the hold is the SDK's ordering of the status
//! type, not the board's transmit queue.
use sedsnet::packet::Packet;
use sedsnet::router::{EndpointHandler, Router, RouterConfig};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type Wire = Arc<Mutex<Vec<Vec<u8>>>>;

/// The FC's status stream reaching the station, with every station-to-FC
/// frame dropped once `drop_back` is set (the reply's ACKs are lost).
/// Returns the status payloads the station received, in order.
fn fc_statuses_after_lost_ack() -> Vec<Vec<u8>> {
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = |n: &Arc<AtomicU64>| {
        let n = n.clone();
        Box::new(move || n.load(Ordering::Relaxed)) as Box<dyn sedsnet::router::Clock + Send + Sync>
    };
    let seen: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let s = seen.clone();
    let status = crate::telemetry_schema::data_type("BOARD_STATE_STATUS");
    let gs = Arc::new(Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            move |p| {
                if p.data_type() == status {
                    s.lock().unwrap().push(p.payload().to_vec());
                }
                Ok(())
            },
        )])
        .with_sender("GS"),
        clock(&now),
    ));
    let fc = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("FC"), clock(&now)));
    let link: crate::comms_config::CommsLinkConfig = serde_json::from_value(
        serde_json::json!({"interface": "serial", "port": "/dev/test-radio", "protocol": "raw_uart"}),
    )
    .unwrap();
    let opts = crate::rocket_router_side_options(&link, false);
    let (up, back): (Wire, Wire) = Default::default();
    let u = up.clone();
    let gs_side = gs.add_side_packed_with_options("rocket_comms", move |b| { u.lock().unwrap().push(b.to_vec()); Ok(()) }, opts.clone());
    let b = back.clone();
    let fc_side = fc.add_side_packed_with_options("radio", move |x| { b.lock().unwrap().push(x.to_vec()); Ok(()) }, opts);
    let drop_back = Arc::new(AtomicBool::new(false));
    let pump = |steps: usize| {
        for _ in 0..steps {
            now.fetch_add(50, Ordering::Relaxed);
            for r in [&gs, &fc] {
                let _ = r.poll_discovery();
                let _ = r.process_all_queues();
            }
            for x in std::mem::take(&mut *back.lock().unwrap()) {
                let _ = gs.rx_packed_from_side(&x, gs_side);
            }
            for x in std::mem::take(&mut *up.lock().unwrap()) {
                if !drop_back.load(Ordering::Relaxed) {
                    let _ = fc.rx_packed_from_side(&x, fc_side);
                }
            }
        }
    };
    gs.announce_discovery().unwrap();
    fc.announce_discovery().unwrap();
    pump(40);
    let send = |first: u8| {
        let mut payload = [0u8; 40];
        payload[0] = first;
        fc.tx_queue(
            Packet::new(
                status,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "FC",
                now.load(Ordering::Relaxed),
                Arc::from(&payload[..]),
            )
            .unwrap(),
        )
        .unwrap();
    };
    // A query reply leaves; the station's ACKs for it never reach the FC.
    drop_back.store(true, Ordering::Relaxed);
    send(1);
    pump(3);
    // 150 ms later the FC latches the Abort and pushes its status.
    send(2);
    // Well inside one retransmit period (400 ms): the push is on air at once.
    pump(4);
    seen.lock().unwrap().clone()
}

#[test]
fn an_abort_push_does_not_wait_for_an_earlier_status_ack() {
    let got = fc_statuses_after_lost_ack();
    let firsts: Vec<u8> = got.iter().map(|p| p[0]).collect();
    assert!(firsts.contains(&1), "the reply reached the station: {firsts:?}");
    assert!(
        firsts.contains(&2),
        "the Abort push reached the station within 200 ms although the earlier reply is unACKed: {firsts:?}"
    );
}
