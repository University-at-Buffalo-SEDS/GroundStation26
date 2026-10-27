//! Bounded replay of actual UART RX bytes, not a re-created sensor workload.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};

#[path = "../comms/virtual_uart.rs"]
mod virtual_uart;

#[derive(Deserialize)]
struct Fixture {
    events: Vec<Event>,
}
#[derive(Deserialize)]
struct Event {
    at_us: u64,
    command: bool,
    payload: String,
    source_rf_tx_us: Option<u64>,
    canonical: bool,
    label: String,
    routine_uplink_heartbeat: bool,
    source_address: Option<u32>,
}
fn selected_source(event: &Event) -> bool {
    event
        .source_rf_tx_us
        .is_some_and(|origin| (26_000_000..=39_000_000).contains(&origin))
}

#[test]
fn captured_canonical_unsequenced_controls_preserve_the_interactive_boundary_tag() {
    crate::telemetry_schema::initialize().unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/full-path-50ms-rx.json")).unwrap();
    let mut verified = 0;
    for event in fixture.events.iter().filter(|event| {
        [
            "FLIGHT_STATE:",
            "FLIGHT_COMMAND:",
            "AV_BAY_UNDERGLOW:",
            "FLIGHT_BUZZER:",
        ]
        .iter()
        .any(|prefix| event.label.starts_with(prefix))
    }) {
        let bytes = STANDARD.decode(&event.payload).unwrap();
        let Some((_, body)) = decode(&bytes) else {
            continue;
        };
        if body.starts_with(b"SDT") {
            continue;
        }
        let info = sedsnet::wire_format::peek_frame_info(body).unwrap();
        assert!(sedsnet::is_reliable_type(info.envelope.ty));
        assert!(info.reliable.is_some_and(|header| header.flags
            & sedsnet::wire_format::RELIABLE_FLAG_UNSEQUENCED
            != 0));
        assert!(crate::comms::closes_radio_packet_batch(255, body));
        let mut relay = Mesh::new(Role::Rrls, 2);
        let health = fixture
            .events
            .iter()
            .find_map(|event| {
                let bytes = STANDARD.decode(&event.payload).unwrap();
                decode(&bytes)
                    .filter(|(kind, body)| *kind == HEALTH && body.len() == 9 && body[8] == 0)
                    .map(|_| bytes.clone())
            })
            .unwrap();
        relay.incoming(true, &health, 0);
        relay.incoming(false, body, 0);
        let transmission = relay.begin_transmission(0).unwrap();
        assert!(transmission.closes_packet_batch());
        verified += 1;
    }
    assert!(
        verified >= 6,
        "exercise multiple real captured commands/retries"
    );
}

#[test]
fn captured_bidirectional_relay_burst_retains_source_and_canonical_deadlines() {
    crate::telemetry_schema::initialize().unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/full-path-50ms-rx.json")).unwrap();
    assert!(fixture.events.len() < 1600);
    let bytes: Vec<_> = fixture
        .events
        .iter()
        .map(|e| STANDARD.decode(&e.payload).unwrap())
        .collect();
    let mut pending = BTreeMap::<(u8, Vec<u8>), VecDeque<usize>>::new();
    let mut outcomes = BTreeMap::<usize, u64>::new();
    let mut relay = Mesh::new(Role::Rrls, 2);
    let mut capacity = crate::comms::rfd_capacity::Capacity::new(64_000, 25, 16);
    let mut wire = virtual_uart::WireDrain::default();
    let wall = Instant::now();
    let mut cursor = 0;
    let mut peak = 0;
    let mut ready_times = Vec::new();
    let mut health_times = Vec::new();
    for now in (0..=41_000u64).step_by(10) {
        while cursor < fixture.events.len() && fixture.events[cursor].at_us <= now * 1000 {
            let event = &fixture.events[cursor];
            let payload = &bytes[cursor];
            if decode(payload).is_some_and(|(kind, _)| kind == HEALTH) {
                health_times.push(now);
            }
            if (26_000_000..=39_000_000).contains(&event.at_us) || selected_source(event) {
                let expected = if event.command {
                    decode(payload)
                        .filter(|(kind, _)| *kind == UP_DATA)
                        .map(|(_, body)| (AV_BAY_DATA, body))
                } else {
                    Some((DOWN_DATA, payload.as_slice()))
                };
                if let Some((kind, body)) = expected {
                    pending
                        .entry((kind, body.to_vec()))
                        .or_default()
                        .push_back(cursor);
                }
            }
            relay.incoming(event.command, payload, now);
            cursor += 1;
        }
        relay.tick(now);
        if (26_000..=39_000).contains(&now) {
            assert!(relay.active(now), "original HEALTH leases remain live");
        }
        capacity.apply_recovery_share(relay.recovery_air_share(now), wall, Some(now));
        peak = peak.max(relay.queue.len());
        assert!(relay.queue.len() <= MAX_FRAMES && relay.bytes <= MAX_BYTES);
        for _ in 0..4 {
            if !wire.ready(now) || !capacity.ready(wall, Some(now)) {
                break;
            }
            let Some(transmission) = relay.begin_transmission(now) else {
                break;
            };
            let frame = transmission.frame();
            let framed_len = frame.bytes.len() + 4;
            if decode(&frame.bytes).is_some_and(|(kind, _)| kind == READY) {
                ready_times.push(now);
            }
            if let Some((kind, body)) = decode(&frame.bytes)
                && let Some(indices) = pending.get_mut(&(kind, body.to_vec()))
                && let Some(index) = indices.pop_front()
            {
                outcomes.insert(index, now);
            }
            wire.committed(now, framed_len, 115_200);
            capacity.committed(wall, Some(now), framed_len);
            if transmission.closes_packet_batch() {
                capacity.close_packet_batch(wall, Some(now));
            }
            relay.finish_transmission(transmission, true);
        }
    }
    let mut failures = Vec::new();
    let mut sources = 0;
    let mut canonical = 0;
    let mut max_source_us = 0;
    let mut max_canonical_us = 0;
    let mut heartbeats = BTreeMap::<u32, (usize, usize, Vec<u64>)>::new();
    for (index, event) in fixture.events.iter().enumerate() {
        if !(26_000_000..=39_000_000).contains(&event.at_us) && !selected_source(event) {
            continue;
        }
        if event.routine_uplink_heartbeat {
            let row = heartbeats.entry(event.source_address.unwrap()).or_default();
            row.0 += 1;
            if let Some(sent) = outcomes.get(&index) {
                row.2.push(*sent);
            } else {
                row.1 += 1;
            }
        }
        let Some(bound) = event
            .source_rf_tx_us
            .filter(|_| selected_source(event))
            .map(|origin| {
                sources += 1;
                (origin, 1500)
            })
            .or_else(|| {
                event.canonical.then(|| {
                    canonical += 1;
                    (event.at_us, 3000)
                })
            })
        else {
            continue;
        };
        match outcomes.get(&index) {
            Some(sent) if sent * 1000 <= bound.0 + bound.1 * 1000 => {
                let delay = sent * 1000 - bound.0;
                if event.source_rf_tx_us.is_some() {
                    max_source_us = max_source_us.max(delay);
                } else {
                    max_canonical_us = max_canonical_us.max(delay);
                }
            }
            sent => failures.push(format!(
                "{} input_us={} origin_us={} offload_ms={sent:?} bound_ms={}",
                event.label, event.at_us, bound.0, bound.1
            )),
        }
    }
    eprintln!(
        "RRLS_CAPTURE_REPLAY sources={sources} canonical={canonical} peak={peak} drops={} max_source_rf_to_offload_us={max_source_us} max_canonical_to_offload_us={max_canonical_us} failures={failures:#?}",
        relay.dropped
    );
    eprintln!(
        "RRLS_CAPTURE_MISSING_OTHER {:?}",
        pending
            .values()
            .flat_map(|indices| indices.iter())
            .map(|index| &fixture.events[*index].label)
            .collect::<Vec<_>>()
    );
    assert!(!sedsnet::is_reliable_type(
        crate::telemetry_schema::data_type("HEARTBEAT")
    ));
    for indices in pending.values() {
        for index in indices {
            assert!(
                fixture.events[*index].routine_uplink_heartbeat,
                "no sensor, metadata or control may be traded for heartbeat: {}",
                fixture.events[*index].label
            );
        }
    }
    for (source, (inputs, drops, times)) in heartbeats {
        let max_gap = times.windows(2).map(|pair| pair[1] - pair[0]).max();
        eprintln!(
            "RRLS_CAPTURE_HEARTBEAT source_address={source} inputs={inputs} drops={drops} outputs={} max_internal_offload_gap_ms={max_gap:?}",
            times.len()
        );
    }
    let largest_gap = |times: &[u64]| {
        times
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .max()
            .unwrap_or(0)
    };
    eprintln!(
        "RRLS_CAPTURE_LIVENESS max_health_gap_ms={} max_ready_gap_ms={}",
        largest_gap(&health_times),
        largest_gap(&ready_times)
    );
    assert!(largest_gap(&health_times) < LEASE_MS && largest_gap(&ready_times) < LEASE_MS);
    assert_eq!(sources, 149, "preserve the actual source workload");
    assert_eq!(
        canonical, 15,
        "preserve both canonical discovery directions"
    );
    assert!(
        failures.is_empty(),
        "captured source/canonical deadlines missed"
    );
}
