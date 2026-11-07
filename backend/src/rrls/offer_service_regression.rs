use super::*;
use std::time::{Duration, Instant};
fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .as_chunks::<2>().0.iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
#[test]
fn accepted_offer_must_reach_paid_service_before_original_probe_deadline() {
    const FLAG: &str = "SEDS_PRIVATE_OFFER_CHILD";
    if std::env::var_os(FLAG).is_none() {
        let r=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","rrls::offer_service_regression::accepted_offer_must_reach_paid_service_before_original_probe_deadline","--nocapture"]).env(FLAG,"1").output().unwrap();
        print!(
            "{}{}",
            String::from_utf8_lossy(&r.stdout),
            String::from_utf8_lossy(&r.stderr)
        );
        assert!(
            r.status.success(),
            "isolated actual-profile OFFER child failed"
        );
        return;
    }
    let link = crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "/unused-owned-native".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    };
    crate::comms::rfd_profile::initialize(&link).unwrap();
    let p = crate::comms::rfd_profile::active()
        .expect("matched production profile must be initialized");
    assert_eq!(p.transport.usable_air_bps, 32000);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("offer_pressure_fixture.json")).unwrap();
    let probe = unhex(fixture["probe"].as_str().unwrap());
    let (_, body) = decode(&probe).unwrap();
    assert_eq!(body[0], 0x10);
    let session = u32::from_le_bytes(body[2..6].try_into().unwrap());
    let seq = u32::from_le_bytes(body[6..10].try_into().unwrap());
    let mut mesh = Mesh::new(Role::Rrls, 7);
    mesh.enable_settings_framing();
    let mut health = session.to_le_bytes().to_vec();
    health.extend(seq.to_le_bytes());
    health.push(0);
    mesh.incoming(true, &encode(HEALTH, &health), 10000);
    while let Some(t) = mesh.begin_transmission(10000) {
        mesh.finish_transmission(t, true);
    }
    mesh.incoming(true, &probe, 10070);
    assert_eq!(
        mesh.settings_framing.as_ref().unwrap().rx_slots(),
        1,
        "real State::incoming must accept PROBE before the scheduling assertion"
    );
    let offered = mesh
        .queue
        .iter()
        .find(|q| decode(&q.frame.bytes).is_some_and(|(k, b)| k == 15 && b[0] == 0x11))
        .expect("accepted PROBE really queues OFFER")
        .frame
        .bytes
        .clone();
    let before = mesh.dropped;
    for value in fixture["pressure"].as_array().unwrap() {
        let chunk = unhex(value.as_str().unwrap());
        assert!(chunk.starts_with(b"SDT\x03"));
        mesh.incoming(false, &chunk, 10070);
    }
    assert_eq!(
        mesh.dropped, before,
        "pressure must not cause admission refusal"
    );
    println!(
        "PROBE accepted at10070 originalcomplete10000 deadline10750; OFFERqueued age0 priority={} queue={} bytes={}",
        mesh.queue
            .iter()
            .find(|q| q.frame.bytes == offered)
            .unwrap()
            .priority,
        mesh.queue.len(),
        mesh.bytes
    );
    let origin = Instant::now();
    let mut capacity = crate::comms::rfd_capacity::Capacity::new(
        p.transport.usable_air_bps,
        35,
        p.transport.frame_overhead_bytes,
    );
    assert_eq!(capacity.airtime_config(), (32000, 35));
    let mut offered_at = None;
    let mut charged = 0usize;
    for now in 10070..=10900 {
        let wall = origin + Duration::from_millis(now - 10070);
        if !capacity.ready(wall, Some(now)) {
            continue;
        }
        let dropped_before = mesh.dropped;
        if let Some(tx) = mesh.begin_transmission(now) {
            let bytes = tx.frame().bytes.clone();
            let (kind, raw) = decode(&bytes).unwrap();
            let is_offer = bytes == offered;
            let wire = bytes.len() + 4;
            println!(
                "COMPLETE ordinary now={now} kind={kind} head={:02x?} wire={wire} offer={is_offer} priority={} charged_before={charged}",
                &raw[..raw.len().min(4)],
                tx.queued.priority
            );
            // Native complete counted/drained service seam: no partial/error acceptance.
            mesh.finish_transmission(tx, true);
            capacity.committed(wall, Some(now), wire);
            charged += wire + 16;
            if is_offer {
                offered_at = Some(now);
            }
        }
        if mesh.dropped != dropped_before {
            println!(
                "REMOVAL now={now} dropped_delta={} original_offer_stillqueued={} cause=queued_expired_after_selector_chose_it",
                mesh.dropped - dropped_before,
                mesh.queue.iter().any(|q| q.frame.bytes == offered)
            );
        }
    }
    println!(
        "RESULT offer_at={offered_at:?} dropped_delta={} charged={charged} remaining={} rx_context={}",
        mesh.dropped - before,
        mesh.queue.len(),
        mesh.settings_framing.as_ref().unwrap().rx_slots()
    );
    assert!(
        offered_at.is_some_and(|t| t < 10750),
        "accepted real OFFER expired behind finite SDT pressure despite paid750ms PROBE negotiation deadline"
    );
}

fn isolated(name: &str) -> bool {
    const KEY: &str = "SEDS_PRIVATE_OFFER_CASE";
    if std::env::var(KEY).ok().as_deref() == Some(name) {
        return false;
    }
    let exact = format!("rrls::offer_service_regression::{name}");
    let r = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(KEY, name)
        .output()
        .unwrap();
    print!(
        "{}{}",
        String::from_utf8_lossy(&r.stdout),
        String::from_utf8_lossy(&r.stderr)
    );
    assert!(r.status.success(), "isolated case {name} failed");
    true
}
fn profile() {
    let link = crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "/unused-owned-native".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    };
    crate::comms::rfd_profile::initialize(&link).unwrap();
    assert_eq!(
        crate::comms::rfd_profile::active()
            .unwrap()
            .transport
            .usable_air_bps,
        32000
    );
}
fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("offer_pressure_fixture.json")).unwrap()
}
fn peer(role: Role) -> (Mesh, Vec<u8>) {
    let mut probe = unhex(fixture()["probe"].as_str().unwrap());
    let (_, body) = decode(&probe).unwrap();
    let session = u32::from_le_bytes(body[2..6].try_into().unwrap());
    let sequence = u32::from_le_bytes(body[6..10].try_into().unwrap());
    let mut mesh = Mesh::new(role, session);
    mesh.enable_settings_framing();
    if role == Role::Rrls {
        let mut h = session.to_le_bytes().to_vec();
        h.extend(sequence.to_le_bytes());
        h.push(0);
        mesh.incoming(true, &encode(HEALTH, &h), 10000);
    } else {
        let mut recovery = Mesh::new(Role::Rrls, 77);
        mesh.tick(10000);
        let health = mesh.pop(10000).unwrap();
        recovery.incoming(true, &health.bytes, 10000);
        recovery.tick(10000);
        let ready = recovery.pop(10000).unwrap();
        mesh.incoming(true, &ready.bytes, 10000);
        let mut b = decode(&probe).unwrap().1.to_vec();
        b[1] |= 0x80;
        b[6..10].copy_from_slice(&mesh.sequence.wrapping_sub(1).to_le_bytes());
        probe = encode(15, &b);
    }
    while let Some(tx) = mesh.begin_transmission(10000) {
        mesh.finish_transmission(tx, true);
    }
    (mesh, probe)
}
fn offer(mesh: &Mesh) -> Vec<u8> {
    mesh.queue
        .iter()
        .find(|q| q.settings_origin == SettingsOrigin::FramingOffer)
        .expect("internally created OFFER admitted")
        .frame
        .bytes
        .clone()
}
fn snapshot(mesh: &Mesh) -> Vec<(u64, u64, bool, bool, u8)> {
    settings_framing::offer_reservation_snapshot(mesh.settings_framing.as_ref().unwrap())
}
fn receive(mesh: &mut Mesh, probe: &[u8], now: u64) {
    mesh.incoming(true, probe, now);
}
fn ack() -> Vec<u8> {
    crate::telemetry_schema::initialize().unwrap();
    let p = sedsnet::packet::Packet::new(
        sedsnet::config::DataType::ReliableAck,
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "RF",
        1,
        42u64.to_le_bytes().to_vec().into(),
    )
    .unwrap();
    sedsnet::wire_format::pack_packet(&p).to_vec()
}
#[test]
fn pending_duplicate_and_refusal_preserve_single_owner_and_original_age() {
    if isolated("pending_duplicate_and_refusal_preserve_single_owner_and_original_age") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    receive(&mut mesh, &probe, 10070);
    let bytes = mesh.bytes;
    let expected = offer(&mesh);
    let rx = snapshot(&mesh);
    receive(&mut mesh, &probe, 10080);
    assert_eq!(mesh.bytes, bytes);
    assert_eq!(mesh.queue.len(), 1);
    assert_eq!(mesh.queue[0].at, 10070);
    assert_eq!(snapshot(&mesh), rx);
    let tx = mesh.begin_transmission(10090).unwrap();
    assert_eq!(tx.frame().bytes, expected);
    receive(&mut mesh, &probe, 10100);
    assert!(mesh.queue.is_empty());
    assert_eq!(mesh.active_frame_count, 1);
    mesh.finish_transmission(tx, false);
    assert_eq!(mesh.queue.len(), 1);
    assert_eq!(mesh.queue[0].at, 10070);
    assert_eq!(mesh.bytes, bytes);
    assert_eq!(snapshot(&mesh), rx);
    let retry = mesh.begin_transmission(10110).unwrap();
    assert_eq!(retry.frame().bytes, expected);
    assert_eq!(
        retry.ordinary_background_service_bytes(),
        0,
        "protocol metadata never changes the ordinary fairness bank"
    );
    assert_eq!(
        retry.successful_ordinary_service_bytes(),
        0,
        "OFFER earns no early-loan service credit"
    );
    mesh.finish_transmission(retry, true);
    assert_eq!(mesh.bytes, 0);
    assert!(!snapshot(&mesh)[0].2);
}
#[test]
fn primary_offer_precedes_pending_reply_and_sdk_ordinary_but_not_urgent() {
    if isolated("primary_offer_precedes_pending_reply_and_sdk_ordinary_but_not_urgent") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Primary);
    receive(&mut mesh, &probe, 10070);
    let expected = offer(&mesh);
    let clock = unhex(fixture()["clock_reply"].as_str().unwrap());
    assert!(mesh.queue_operating_mode_reply(&clock, 10070));
    let control = ack();
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(AV_BAY_DATA, &control),
        },
        254,
        10070,
    );
    let first = mesh
        .begin_primary_background_transmission(10071, 0, true, false)
        .unwrap();
    assert_ne!(first.frame().bytes, expected);
    assert!(first.is_receipt());
    mesh.finish_transmission(first, true);
    assert!(
        mesh.begin_primary_background_transmission(10072, 0, false, false)
            .is_none(),
        "OFFER must never borrow early Capacity"
    );
    let tx = mesh
        .begin_primary_background_transmission(10073, 0, true, false)
        .unwrap();
    assert_eq!(tx.frame().bytes, expected);
    assert_eq!(tx.ordinary_background_service_bytes(), 0);
    assert_eq!(tx.successful_ordinary_service_bytes(), 0);
    mesh.finish_transmission(tx, true);
    assert!(
        mesh.queue
            .iter()
            .any(|q| q.settings_origin == SettingsOrigin::Reply)
    );
    assert!(
        mesh.begin_primary_background_transmission(10074, 0, true, false)
            .is_none(),
        "an owed SDK turn still blocks ordinary ClockReply"
    );
    assert!(
        mesh.begin_primary_background_transmission(10075, 0, true, true)
            .is_some(),
        "ordinary ClockReply keeps its later service turn"
    );
}
#[test]
fn offer_expires_with_fixed_reservation_and_raw_kind15_gets_no_privilege() {
    if isolated("offer_expires_with_fixed_reservation_and_raw_kind15_gets_no_privilege") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    receive(&mut mesh, &probe, 10070);
    let original = snapshot(&mesh);
    receive(&mut mesh, &probe, 10400);
    assert_eq!(snapshot(&mesh), original);
    let fixed_unused_expiry = original[0].1;
    assert_eq!(
        fixed_unused_expiry, 12070,
        "existing RX-unused lifetime remains two seconds, independent of sender750ms"
    );
    let mut health = mesh.primary_session.unwrap().to_le_bytes().to_vec();
    health.extend(mesh.primary_sequence.wrapping_add(1).to_le_bytes());
    health.push(0);
    mesh.incoming(true, &encode(HEALTH, &health), 11900);
    mesh.tick(11900);
    while let Some(tx) = mesh.begin_transmission_at_priority(11900, 255) {
        mesh.finish_transmission(tx, true);
    }
    assert_eq!(
        snapshot(&mesh)[0].1,
        fixed_unused_expiry,
        "fresh HEALTH never extends the reservation"
    );
    assert!(mesh.begin_transmission(fixed_unused_expiry).is_none());
    assert!(mesh.queue.is_empty());
    assert_eq!(mesh.dropped, 1);
    assert!(!snapshot(&mesh)[0].2);
    let (mut rawmesh, probe) = peer(Role::Rrls);
    receive(&mut rawmesh, &probe, 10070);
    let bytes = offer(&rawmesh);
    rawmesh.clear();
    assert!(!snapshot(&rawmesh)[0].2);
    rawmesh.enqueue(
        Outgoing {
            command: true,
            bytes: bytes.clone(),
        },
        0,
        10080,
    );
    assert_eq!(rawmesh.queue[0].settings_origin, SettingsOrigin::None);
    let chunk = unhex(fixture()["pressure"][0].as_str().unwrap());
    rawmesh.incoming(false, &chunk, 10080);
    let tx = rawmesh.begin_transmission(10081).unwrap();
    assert_ne!(
        tx.frame().bytes,
        bytes,
        "unowned raw kind15 cannot jump ahead of queued SDT data"
    );
}
#[test]
fn frame_refusal_and_eviction_release_pending_and_allow_bounded_retry() {
    if isolated("frame_refusal_and_eviction_release_pending_and_allow_bounded_retry") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    for _ in 0..MAX_FRAMES {
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, &[7; 16]),
            },
            254,
            10060,
        );
    }
    receive(&mut mesh, &probe, 10070);
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    assert!(
        !snapshot(&mesh)[0].2,
        "failed queue admission must not pin a shadow OFFER"
    );
    assert!(
        !mesh
            .queue
            .iter()
            .any(|q| q.settings_origin == SettingsOrigin::FramingOffer)
    );
    let tx = mesh.begin_transmission(10080).unwrap();
    mesh.finish_transmission(tx, true);
    receive(&mut mesh, &probe, 10090);
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    assert!(snapshot(&mesh)[0].2);
    let original_expiry = snapshot(&mesh)[0].0;
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(DOWN_DATA, &[8; 16]),
        },
        254,
        10100,
    );
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    assert!(
        !snapshot(&mesh)[0].2,
        "urgent admission may evict the queued OFFER and releases its marker"
    );
    let tx = mesh.begin_transmission(10110).unwrap();
    mesh.finish_transmission(tx, true);
    receive(&mut mesh, &probe, 10120);
    assert!(snapshot(&mesh)[0].2);
    assert_eq!(
        snapshot(&mesh)[0].0,
        original_expiry,
        "retry does not extend receiver lifetime"
    );
}
#[test]
fn byte_refusal_never_creates_stuck_pending_or_extra_reservation() {
    if isolated("byte_refusal_never_creates_stuck_pending_or_extra_reservation") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    loop {
        let free = MAX_BYTES - mesh.bytes - mesh.settings_reserved_bytes();
        if free <= 410 {
            assert_eq!(free, 410);
            break;
        }
        let frame = (free - 410).min(MAX_PAYLOAD + 8);
        assert!(frame >= 8);
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(DOWN_DATA, &vec![9; frame - 8]),
            },
            254,
            10060,
        );
    }
    receive(&mut mesh, &probe, 10070);
    assert_eq!(snapshot(&mesh).len(), 1, "PROBE context itself fits");
    assert!(
        !snapshot(&mesh)[0].2,
        "wrapped OFFER cannot fit after the context allocation"
    );
    assert!(mesh.bytes + mesh.settings_reserved_bytes() <= MAX_BYTES);
    let tx = mesh.begin_transmission(10080).unwrap();
    mesh.finish_transmission(tx, true);
    receive(&mut mesh, &probe, 10090);
    assert!(snapshot(&mesh)[0].2);
    assert_eq!(snapshot(&mesh).len(), 1);
    assert!(mesh.bytes + mesh.settings_reserved_bytes() <= MAX_BYTES);
}
#[test]
fn four_receiver_slots_and_pending_duplicates_bound_metadata_without_credit() {
    if isolated("four_receiver_slots_and_pending_duplicates_bound_metadata_without_credit") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    let b = decode(&probe).unwrap().1;
    for n in 0..5u8 {
        let mut p = b.to_vec();
        p[10] ^= n;
        receive(&mut mesh, &encode(15, &p), 10070 + u64::from(n));
    }
    assert_eq!(snapshot(&mesh).len(), 4);
    assert_eq!(mesh.queue.len(), 4);
    let owned = mesh.bytes;
    for _ in 0..100 {
        for n in 0..4u8 {
            let mut p = b.to_vec();
            p[10] ^= n;
            receive(&mut mesh, &encode(15, &p), 10100);
        }
    }
    assert_eq!(mesh.queue.len(), 4);
    assert_eq!(mesh.bytes, owned);
    assert!(mesh.bytes + mesh.settings_reserved_bytes() <= MAX_BYTES);
    for now in 10101..10105 {
        let tx = mesh.begin_transmission(now).unwrap();
        assert_eq!(tx.ordinary_background_service_bytes(), 0);
        assert_eq!(tx.successful_ordinary_service_bytes(), 0);
        mesh.finish_transmission(tx, true);
    }
    assert!(mesh.queue.is_empty());
    assert_eq!(std::mem::size_of::<SettingsOrigin>(), 1);
    assert_eq!(
        settings_framing::reservation_layout_sizes().0,
        settings_framing::reservation_layout_sizes().1
    );
    #[cfg(target_pointer_width = "64")]
    {
        assert_eq!(std::mem::size_of::<Queued>(), 48);
        assert_eq!(std::mem::size_of::<Option<Queued>>(), 48);
    }
}

#[test]
fn full_ordinary_pressure_cannot_evict_live_offer_but_urgent_remains_admissible() {
    if isolated("full_ordinary_pressure_cannot_evict_live_offer_but_urgent_remains_admissible") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    let chunk = unhex(fixture()["pressure"][0].as_str().unwrap());
    for _ in 0..MAX_FRAMES {
        mesh.incoming(false, &chunk, 10060);
    }
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    let before = mesh.dropped;
    receive(&mut mesh, &probe, 10070);
    let expected = offer(&mesh);
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    assert_eq!(
        mesh.dropped,
        before + 1,
        "existing bounded eviction is visible; no capacity grows"
    );
    for now in 10071..10300 {
        mesh.incoming(false, &chunk, now);
    }
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    assert!(mesh.queue.iter().any(|q| q.frame.bytes == expected));
    assert!(snapshot(&mesh)[0].2);
    let control = ack();
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(DOWN_DATA, &control),
        },
        254,
        10300,
    );
    assert_eq!(mesh.queue.len(), MAX_FRAMES);
    let urgent = mesh.begin_transmission(10301).unwrap();
    assert!(urgent.is_receipt());
    mesh.finish_transmission(urgent, true);
    let reply = mesh.begin_transmission(10302).unwrap();
    assert_eq!(reply.frame().bytes, expected);
    assert_eq!(reply.ordinary_background_service_bytes(), 0);
    mesh.finish_transmission(reply, true);
    assert!(!snapshot(&mesh)[0].2);
    assert!(mesh.bytes + mesh.settings_reserved_bytes() <= MAX_BYTES);
}

#[test]
fn clear_and_session_change_retire_unsent_offer_without_touching_active_refusal_rules() {
    if isolated(
        "clear_and_session_change_retire_unsent_offer_without_touching_active_refusal_rules",
    ) {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    receive(&mut mesh, &probe, 10070);
    mesh.clear();
    assert!(!snapshot(&mesh)[0].2);
    assert!(mesh.queue.is_empty());
    receive(&mut mesh, &probe, 10080);
    let expected = offer(&mesh);
    let tx = mesh.begin_transmission(10090).unwrap();
    assert_eq!(tx.frame().bytes, expected);
    receive(&mut mesh, &probe, 10100);
    assert!(mesh.queue.is_empty());
    mesh.finish_transmission(tx, false);
    assert_eq!(mesh.queue.len(), 1);
    assert!(
        snapshot(&mesh)[0].2,
        "refusal preserves the retained physical owner"
    );
    // Genuine session replacement invalidates its fixed receiver context;
    // no new queue priority is inferred from the wire tag alone.
    mesh.primary_session = mesh.primary_session.map(|s| s.wrapping_add(1));
    mesh.observe_settings_framing(10110);
    assert!(snapshot(&mesh).is_empty());
    assert!(mesh.begin_transmission(10111).is_none());
    assert!(mesh.queue.is_empty());
    assert_eq!(mesh.bytes, 0);
}

#[test]
fn completed_offer_replays_are_bounded_but_one_lost_response_can_retry() {
    if isolated("completed_offer_replays_are_bounded_but_one_lost_response_can_retry") {
        return;
    }
    profile();
    let (mut mesh, probe) = peer(Role::Rrls);
    receive(&mut mesh, &probe, 10070);
    let original_lifetime = snapshot(&mesh)[0];
    let first = mesh.begin_transmission(10071).unwrap();
    let original_bytes = first.frame().bytes.clone();
    mesh.finish_transmission(first, true); // counted completion, deliberately lost on the modeled air path
    assert_eq!(snapshot(&mesh)[0].4, 1);
    receive(&mut mesh, &probe, 10445); // the existing one375ms same-byte sender retry, with the same incoming transit
    let second = mesh.begin_transmission(10446).unwrap();
    assert_eq!(second.frame().bytes, original_bytes);
    mesh.finish_transmission(second, false);
    assert_eq!(
        snapshot(&mesh)[0].4,
        1,
        "refusal does not spend a completed-response allowance"
    );
    let same_owner = mesh.begin_transmission(10447).unwrap();
    assert_eq!(same_owner.frame().bytes, original_bytes);
    mesh.finish_transmission(same_owner, true);
    assert_eq!(snapshot(&mesh)[0].4, 2);
    for now in 10448..10648 {
        receive(&mut mesh, &probe, now);
    }
    assert!(
        mesh.queue.is_empty(),
        "one RX slot cannot mint an unlimited metadata stream"
    );
    assert_eq!(snapshot(&mesh)[0].0, original_lifetime.0);
    assert_eq!(snapshot(&mesh)[0].1, original_lifetime.1);
    assert_eq!(snapshot(&mesh).len(), 1);
}
