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
        // HEALTH/discovery at255 may not hide a real control254 when only
        // critical borrowing is available. Ordinary opaqueSDT retains ownership.
        relay.enqueue(
            Outgoing {
                command: true,
                bytes: encode(HEALTH, &[0; 9]),
            },
            255,
            0,
        );
        relay.enqueue(
            Outgoing {
                command: false,
                bytes: encode(DOWN_DATA, b"SDT\x03opaque"),
            },
            253,
            0,
        );
        let owned_before = relay.bytes;
        let transmission = relay.begin_critical_transmission(0, 0).unwrap();
        assert!(decode(&transmission.frame().bytes).is_some_and(|(_, raw)| raw == body));
        relay.finish_transmission(transmission, false);
        assert_eq!(relay.bytes, owned_before, "refusal retains byte ownership");
        let transmission = relay.begin_critical_transmission(0, 0).unwrap();
        assert!(transmission.closes_packet_batch());
        relay.finish_transmission(transmission, true);
        let remaining = relay.bytes;
        let dropped = relay.dropped;
        assert!(relay.begin_critical_transmission(10_000, 0).is_none());
        assert_eq!(
            relay.bytes, remaining,
            "critical-only selection cannot consume routine data"
        );
        assert_eq!(
            relay.dropped, dropped,
            "critical-only polling cannot expire ordinary data"
        );
        let transmission = relay.begin_transmission(0).unwrap();
        assert!(!transmission.closes_packet_batch());
        verified += 1;
    }
    assert!(
        verified >= 6,
        "exercise multiple real captured commands/retries"
    );
}

#[test]
fn captured_control_can_borrow_after_184ms_bulk_debt_with_health_pending() {
    crate::telemetry_schema::initialize().unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/full-path-50ms-rx.json")).unwrap();
    let command = fixture
        .events
        .iter()
        .filter(|event| event.label.starts_with("AV_BAY_UNDERGLOW:"))
        .find_map(|event| {
            let bytes = STANDARD.decode(&event.payload).unwrap();
            let (_, body) = decode(&bytes)?;
            (!body.starts_with(b"SDT")).then(|| body.to_vec())
        })
        .unwrap();
    assert!(crate::comms::closes_radio_packet_batch(254, &command));
    let run = |borrow: bool| {
        let wall = std::time::Instant::now();
        let mut mesh = Mesh::new(Role::Rrls, 2);
        mesh.enqueue(
            Outgoing {
                command: false,
                bytes: encode(DOWN_DATA, &command),
            },
            254,
            9000,
        );
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: encode(HEALTH, &[0; 9]),
            },
            255,
            9000,
        );
        let mut capacity = crate::comms::rfd_capacity::Capacity::new(64_000, 15, 16);
        let mut wire = virtual_uart::WireDrain::default();
        capacity.committed(wall, Some(8990), 204);
        wire.committed(8990, 204, 115_200);
        capacity.drained_virtual(wire.stop_us());
        for now in 9000..9300 {
            if !wire.ready(now) {
                continue;
            }
            let normal = capacity.ready(wall, Some(now));
            let candidate = if normal {
                mesh.begin_transmission(now)
            } else if borrow {
                mesh.begin_critical_transmission(now, 0)
            } else {
                None
            };
            let Some(transmission) = candidate else {
                continue;
            };
            if !capacity.ready_for(
                wall,
                Some(now),
                borrow && transmission.closes_packet_batch(),
            ) {
                mesh.finish_transmission(transmission, false);
                continue;
            }
            let is_command = transmission.closes_packet_batch();
            let length = transmission.frame().bytes.len() + 4;
            wire.committed(now, length, 115_200);
            capacity.committed(wall, Some(now), length);
            capacity.drained_virtual(wire.stop_us());
            mesh.finish_transmission(transmission, true);
            if is_command {
                return now;
            }
        }
        panic!("captured control never admitted")
    };
    let baseline = run(false);
    let borrowed = run(true);
    eprintln!("CAPTURED_CRITICAL_BULK_DEBT baseline_ms={baseline} borrowed_ms={borrowed}");
    assert!(
        baseline >= 9200,
        "HEALTH plus existing bulk debt reproduce delayed admission"
    );
    assert_eq!(
        borrowed, 9019,
        "one bounded155ms loan preserves the184ms debt and UART stopbit gate"
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
        let mut serviced_frames = 0;
        while serviced_frames < 4 {
            if !wire.ready(now) {
                break;
            }
            let candidate = relay.begin_bounded_transmission(
                now,
                0,
                !capacity.ready(wall, Some(now)),
                4 - serviced_frames,
            );
            let Some(transmission) = candidate else {
                break;
            };
            if !capacity.ready_for(wall, Some(now), transmission.closes_packet_batch()) {
                relay.finish_bounded_transmission(transmission, false);
                break;
            }
            let frames = [Some(transmission.frame()), transmission.follower_frame()];
            let mut lengths = [0; 2];
            for (i, frame) in frames.into_iter().enumerate() {
                let Some(frame) = frame else { continue };
                lengths[i] = frame.bytes.len() + 4;
                if decode(&frame.bytes).is_some_and(|(kind, _)| kind == READY) {
                    ready_times.push(now);
                }
                if let Some((kind, body)) = decode(&frame.bytes)
                    && let Some(indices) = pending.get_mut(&(kind, body.to_vec()))
                    && let Some(index) = indices.pop_front()
                {
                    outcomes.insert(index, now);
                }
            }
            let count = 1 + usize::from(lengths[1] > 0);
            wire.committed(now, lengths.iter().sum(), 115_200);
            if lengths[1] > 0 {
                capacity.committed_command_batch(wall, Some(now), lengths, lengths.iter().sum());
            } else {
                capacity.committed(wall, Some(now), lengths[0]);
            }
            capacity.drained_virtual(wire.stop_us());
            if transmission.closes_packet_batch() {
                let receipt_promoted = if let Some(contracts) = transmission.batch_contracts() {
                    capacity.close_control_batch(wall, Some(now), contracts).1
                } else {
                    let contract = transmission.boundary_contract();
                    capacity.close_packet_batch_for_contract(wall, Some(now), contract)
                        && contract.is_some_and(|(_, receipt)| receipt)
                };
                if receipt_promoted {
                    relay.note_receipt_packet_boundary(capacity.packet_idle_service_bytes());
                }
            }
            capacity.note_successful_ordinary_service(transmission.successful_ordinary_service_bytes());
            relay.finish_bounded_transmission(transmission, true);
            serviced_frames += count;
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

#[test]
fn ordinary_source_progress_survives_routine_heartbeat_without_metadata() {
    crate::telemetry_schema::initialize().unwrap();
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/full-path-50ms-rx.json")).unwrap();
    let event = fixture
        .events
        .iter()
        .find(|e| e.label.starts_with("BAROMETER_DATA:"))
        .unwrap();
    let source = STANDARD.decode(&event.payload).unwrap();
    let heartbeat = fixture
        .events
        .iter()
        .filter(|e| e.routine_uplink_heartbeat)
        .find_map(|e| {
            let bytes = STANDARD.decode(&e.payload).unwrap();
            let (kind, body) = decode(&bytes)?;
            (kind == UP_DATA).then(|| encode(AV_BAY_DATA, body))
        })
        .unwrap();
    let mut mesh = Mesh::new(Role::Rrls, 2);
    mesh.enqueue(
        Outgoing {
            command: false,
            bytes: encode(DOWN_DATA, &source),
        },
        0,
        0,
    );
    let mut delivered = None;
    // Fixed available-admission opportunities, not a new rate setting. Every
    // opportunity has one fresh routine packet, so strict priority cannot
    // claim eventual sensor service merely from its 1500ms retention bound.
    for now in (50..=2000u64).step_by(50) {
        mesh.enqueue(
            Outgoing {
                command: true,
                bytes: heartbeat.clone(),
            },
            253,
            now,
        );
        let tx = mesh.begin_transmission(now).unwrap();
        if decode(&tx.frame().bytes).is_some_and(|(kind, body)| kind == DOWN_DATA && body == source)
        {
            delivered = Some(now);
        }
        mesh.finish_transmission(tx, true);
    }
    assert!(
        delivered.is_some_and(|at| at <= 1500),
        "routineHB253starvedacceptedFCsource0: {delivered:?}"
    );
}

#[test]
fn ordinary_fifo_preserves_nondata_priority_and_equal_priority_age() {
    let mut mesh = Mesh::new(Role::Rrls, 2);
    // Voice1 retains its priority over sensor0 when it is the generic winner.
    mesh.enqueue(
        Outgoing {
            command: false,
            bytes: encode(DOWN_DATA, b"sensor"),
        },
        0,
        0,
    );
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(VOICE, b"voice"),
        },
        1,
        1,
    );
    let tx = mesh.begin_transmission(2).unwrap();
    assert_eq!(decode(&tx.frame().bytes).unwrap().0, VOICE);
    mesh.finish_transmission(tx, true);
    let tx = mesh.begin_transmission(3).unwrap();
    assert_eq!(decode(&tx.frame().bytes).unwrap().0, DOWN_DATA);
    mesh.finish_transmission(tx, true);
    // An older settings0 winner keeps its equal-priority queue age; selecting
    // FIFO inside ordinary does not promote sensor0 across a different lane.
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(SETTINGS, b"settings"),
        },
        0,
        4,
    );
    mesh.enqueue(
        Outgoing {
            command: false,
            bytes: encode(DOWN_DATA, b"sensor2"),
        },
        0,
        5,
    );
    let tx = mesh.begin_transmission(6).unwrap();
    assert_eq!(decode(&tx.frame().bytes).unwrap().0, SETTINGS);
    mesh.finish_transmission(tx, true);
    let tx = mesh.begin_transmission(7).unwrap();
    assert_eq!(decode(&tx.frame().bytes).unwrap().0, DOWN_DATA);
    mesh.finish_transmission(tx, true);
}
