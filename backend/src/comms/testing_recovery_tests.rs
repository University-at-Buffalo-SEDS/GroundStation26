//! Actual production DummyComms path, never coordinator fixture proofs.
use super::*;
use sedsnet::router::{EndpointHandler, RouterConfig};
use std::sync::{Arc, Mutex};
#[test]
fn deployed_dummy_returns_correlated_board_query() {
    crate::telemetry_schema::initialize().unwrap();
    let received = Arc::new(Mutex::new(Vec::<Packet>::new()));
    let capture = received.clone();
    let host = Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            move |packet| {
                capture.lock().unwrap().push(packet.clone());
                Ok(())
            },
        )])
        .with_sender("GS"),
    );
    let side = host.add_side_packed("dummy-proof", |_| Ok(()));
    let mut link = DummyComms::new("Umbilical Comms");
    link.set_side_id(side);
    let mut body = [0u8; 32];
    body[0] = 1;
    body[4..12].copy_from_slice(&123_u64.to_le_bytes());
    body[24..32].copy_from_slice(&456_u64.to_le_bytes());
    let query = Packet::new(
        crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB")],
        "GS",
        crate::telemetry_task::get_current_timestamp_ms(),
        Arc::from(body),
    )
    .unwrap();
    link.send_data(&sedsnet::wire_format::pack_packet(&query))
        .unwrap();
    for _ in 0..100 {
        link.recv_packet(&host, &mut |_| {}).unwrap();
        host.process_all_queues().unwrap();
    }
    let rows = received.lock().unwrap();
    let replies: Vec<_> = rows
        .iter()
        .filter(|p| p.data_type() == crate::telemetry_schema::data_type("BOARD_STATE_STATUS"))
        .collect();
    assert_eq!(
        replies.len(),
        1,
        "deployed DummyComms must answer the real application QUERY"
    );
    assert_eq!(replies[0].sender(), "AB");
    assert_eq!(replies[0].payload().len(), 40);
    assert_eq!(&replies[0].payload()[4..12], &123_u64.to_le_bytes());
    assert_eq!(&replies[0].payload()[32..40], &456_u64.to_le_bytes());
}
