use super::*;
#[test]
fn fallback_forwards_gs_heartbeat_identity_without_manufacturing_watchdog_pulses() {
    crate::telemetry_schema::initialize().unwrap();
    let packet = sedsnet::packet::Packet::new(
        crate::telemetry_schema::data_type("HEARTBEAT"),
        &[
            crate::telemetry_schema::endpoint("HEART_BEAT"),
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
        ],
        "GS",
        123,
        Vec::<u8>::new().into(),
    )
    .unwrap();
    let wire = sedsnet::wire_format::pack_packet(&packet);
    let expected_source = sedsnet::wire_format::peek_envelope(&wire)
        .unwrap()
        .sender
        .to_string();
    let mut relay = Mesh::new(Role::Rrls, 2);
    assert!(
        relay.local_tx(false, &wire, 3000).is_none(),
        "observer has no local GS transmit authority"
    );
    let mut health = 1u32.to_le_bytes().to_vec();
    health.extend_from_slice(&1u32.to_le_bytes());
    health.push(0); // primary has lost direct rocket reception
    assert!(
        relay
            .incoming(true, &encode(HEALTH, &health), 3000)
            .is_none()
    );
    assert!(relay.active(3000));
    assert!(
        relay.pop(3000).is_none(),
        "Mesh HEALTH must not become a SEDS heartbeat"
    );
    let observer = relay.incoming(true, &encode(UP_DATA, &wire), 3001).unwrap();
    assert_eq!(observer.bytes.as_slice(), wire.as_ref());
    let forwarded = relay.pop(3001).unwrap();
    let (kind, body) = decode(&forwarded.bytes).unwrap();
    assert_eq!(kind, AV_BAY_DATA);
    assert_eq!(
        body,
        wire.as_ref(),
        "forwarding preserves the exact GS packet and endpoints"
    );
    assert_eq!(
        sedsnet::wire_format::peek_envelope(body)
            .unwrap()
            .sender
            .as_ref(),
        expected_source.as_str()
    );
    assert!(
        !relay.active(5000),
        "only the original HEALTH lease controls relay authority"
    );
    assert!(relay.local_tx(false, &wire, 5000).is_none());
}
