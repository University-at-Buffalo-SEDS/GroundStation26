use super::*;
use sedsnet::{packet::Packet, wire_format as serialize};
use std::collections::{BTreeMap, BTreeSet};
fn packet(name: &str, endpoint: &str, sender: &str, now: u64, payload: Vec<u8>) -> Vec<u8> {
    serialize::pack_packet(
        &Packet::new(
            crate::telemetry_schema::data_type(name),
            &[crate::telemetry_schema::endpoint(endpoint)],
            sender,
            now,
            Arc::from(payload),
        )
        .unwrap(),
    )
    .to_vec()
}
#[test]
fn continuous_protected_under_capacity_service_baseline() {
    crate::telemetry_schema::initialize().unwrap();
    for control_period in [100u64, 50] {
        let wall = Instant::now();
        let mut mesh = Mesh::new(Role::Rrls, 2);
        let mut cap = crate::comms::rfd_capacity::Capacity::new(64_000, 25, 16);
        let mut wire = receipt_test_wire::WireDrain::default();
        let mut pending = BTreeMap::<u64, (u64, bool)>::new();
        let mut seen = BTreeSet::new();
        let mut source_ages = Vec::new();
        let mut control_ages = Vec::new();
        let mut source_times = Vec::new();
        let mut offered = [0usize; 2];
        let mut charged = 0;
        let mut peak = 0;
        let mut pairs = 0;
        for now in 0..=10000u64 {
            if now % 500 == 0 {
                let mut health = 1u32.to_le_bytes().to_vec();
                health.extend_from_slice(&(now as u32 / 500).to_le_bytes());
                health.push(0);
                mesh.incoming(true, &encode(HEALTH, &health), now);
            }
            if now <= 8000 && now % 200 == 0 {
                let bytes = packet("BAROMETER_DATA", "GROUND_STATION", "FC", now, vec![0; 12]);
                let id = serialize::unpack_packet(&bytes).unwrap().packet_id();
                assert!(seen.insert(id));
                pending.insert(id, (now, false));
                offered[0] += bytes.len() + 12 + 16;
                mesh.incoming(false, &bytes, now);
            }
            if now <= 8000 && now % control_period == 0 {
                let bytes = packet("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", "GS", now, vec![14]);
                let id = serialize::unpack_packet(&bytes).unwrap().packet_id();
                assert!(seen.insert(id));
                pending.insert(id, (now, true));
                offered[1] += bytes.len() + 12 + 16;
                mesh.incoming(true, &encode(UP_DATA, &bytes), now);
            }
            mesh.tick(now);
            cap.apply_recovery_share(mesh.recovery_air_share(now), wall, Some(now));
            peak = peak.max(mesh.queue.len());
            assert!(
                mesh.queue.len() + usize::from(mesh.active_frame_count) <= MAX_FRAMES
                    && mesh.bytes <= MAX_BYTES
            );
            let mut count = 0;
            while count < 4 && wire.ready(now) {
                let Some(tx) =
                    mesh.begin_bounded_transmission(now, 0, !cap.ready(wall, Some(now)), 4 - count)
                else {
                    break;
                };
                if !cap.ready_for(wall, Some(now), tx.closes_packet_batch()) {
                    mesh.finish_bounded_transmission(tx, false);
                    break;
                }
                let mut lengths = [0; 2];
                for (i, frame) in [Some(tx.frame()), tx.follower_frame()]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(frame) = frame {
                        lengths[i] = frame.bytes.len() + 4;
                        charged += lengths[i] + 16;
                        if let Some((kind, body)) = decode(&frame.bytes)
                            && matches!(kind, DOWN_DATA | AV_BAY_DATA)
                            && let Ok(p) = serialize::unpack_packet(body)
                            && let Some((origin, control)) = pending.remove(&p.packet_id())
                        {
                            if control {
                                control_ages.push(now - origin)
                            } else {
                                source_ages.push(now - origin);
                                source_times.push(now)
                            }
                        }
                    }
                }
                let n = 1 + usize::from(lengths[1] > 0);
                pairs += usize::from(n == 2);
                wire.committed(now, lengths.iter().sum(), 115200);
                if n == 2 {
                    cap.committed_command_batch(wall, Some(now), lengths, lengths.iter().sum())
                } else {
                    cap.committed(wall, Some(now), lengths[0])
                }
                cap.drained_virtual(wire.stop_us());
                if tx.closes_packet_batch() {
                    let receipt = if let Some(contracts) = tx.batch_contracts() {
                        cap.close_control_batch(wall, Some(now), contracts).1
                    } else {
                        let c = tx.boundary_contract();
                        cap.close_packet_batch_for_contract(wall, Some(now), c)
                            && c.is_some_and(|(_, r)| r)
                    };
                    if receipt {
                        mesh.note_receipt_packet_boundary(cap.packet_idle_service_bytes())
                    }
                }
                cap.note_successful_ordinary_service(tx.successful_ordinary_service_bytes());
                mesh.finish_bounded_transmission(tx, true);
                count += n;
            }
        }
        let missing_source = pending.values().filter(|(_, c)| !c).count();
        let missing_control = pending.values().filter(|(_, c)| *c).count();
        let source_max = source_ages.iter().max().copied().unwrap_or(0);
        let control_max = control_ages.iter().max().copied().unwrap_or(0);
        let source_gap = source_times
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0);
        eprintln!(
            "CONTINUOUS_BASELINE control_period_ms={control_period} source_period_ms=200 offered_source_charged={} offered_control_charged={} emitted_source={} emitted_control={} delivered_source={} delivered_control={} missing_source={missing_source} missing_control={missing_control} source_max_ms={source_max} source_max_gap_ms={source_gap} control_max_ms={control_max} source_late={} control_late={} actual_charged={charged} peak={peak} dropped={} pairs={pairs}",
            offered[0],
            offered[1],
            41,
            8000 / control_period + 1,
            source_ages.len(),
            control_ages.len(),
            source_ages.iter().filter(|&&a| a > 1500).count(),
            control_ages.iter().filter(|&&a| a > 500).count(),
            mesh.dropped
        );
        assert_eq!(
            missing_source, 0,
            "under-capacitybaseline sourceownershiploss"
        );
        assert_eq!(missing_control, 0, "baseline controlownershiploss");
        assert!(source_max <= 1500, "baseline sourceage");
        assert!(control_max <= 500, "baseline controlage");
    }
}

#[test]
fn continuous_three_lane_large_report_and_metadata_baseline() {
    crate::telemetry_schema::initialize().unwrap();
    sedsnet::config::register_data_type_with_description(
        "RFD_ORDINARY_LARGE_AUDIT",
        "test",
        sedsnet::MessageElement::Static(
            144,
            sedsnet::MessageDataType::UInt8,
            sedsnet::MessageClass::Data,
        ),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        sedsnet::ReliableMode::None,
        0,
    )
    .unwrap();
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/full-path-50ms-rx.json")).unwrap();
    let metadata = fixture["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["canonical"].as_bool() == Some(true))
        .find_map(|e| {
            let bytes = STANDARD.decode(e["payload"].as_str().unwrap()).unwrap();
            let body = decode(&bytes).map_or(bytes.as_slice(), |(_, b)| b);
            serialize::unpack_packet(body)
                .ok()
                .map(|p| p.payload().to_vec())
        })
        .unwrap();
    for (control_period, preearned, phase) in [
        (100u64, false, 0u64),
        (100, true, 0),
        (100, true, 25),
        (100, true, 50),
        (100, true, 75),
        (50, false, 0),
        (50, true, 0),
        (50, true, 10),
        (50, true, 25),
        (50, true, 40),
    ] {
        let wall = Instant::now();
        let mut mesh = Mesh::new(Role::Rrls, 2);
        let mut cap = crate::comms::rfd_capacity::Capacity::new(64_000, 25, 16);
        let mut wire = receipt_test_wire::WireDrain::default();
        let mut pending = BTreeMap::<u64, (u64, u8)>::new();
        let mut seen = BTreeSet::new();
        let mut source_ages = Vec::new();
        let mut control_ages = Vec::new();
        let mut source_times = Vec::new();
        let mut offered = [0usize; 3];
        let mut metadata_ages = Vec::new();
        let mut charged = 0;
        let mut peak = 0;
        let mut pairs = 0;
        if preearned {
            cap.apply_recovery_share(Some(35), wall, Some(0));
            wire.committed(0, 176, 115200);
            cap.committed(wall, Some(0), 176);
            cap.drained_virtual(wire.stop_us());
            charged += 192;
            cap.note_successful_ordinary_service(176);
        }
        eprintln!(
            "THREE_LANE_PHASE period={control_period} preearned={preearned} control_phase_ms={phase}"
        );
        for now in 0..=10000u64 {
            if now % 500 == 0 {
                let mut health = 1u32.to_le_bytes().to_vec();
                health.extend_from_slice(&(now as u32 / 500).to_le_bytes());
                health.push(0);
                mesh.incoming(true, &encode(HEALTH, &health), now);
            }
            if now <= 8000 && now % 200 == 0 {
                let bytes = packet("RFD_ORDINARY_LARGE_AUDIT", "GROUND_STATION", "FC", now, {
                    let mut x = now.wrapping_add(0x243f6a8885a308d3u64);
                    (0..144)
                        .map(|_| {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                            x as u8
                        })
                        .collect()
                });
                assert!(
                    bytes.len() + 12 >= 176,
                    "large-source wire unexpectedly compressed"
                );
                let id = serialize::unpack_packet(&bytes).unwrap().packet_id();
                assert!(seen.insert(id));
                pending.insert(id, (now, 0));
                offered[0] += bytes.len() + 12 + 16;
                mesh.incoming(false, &bytes, now);
            }
            if now <= 8000 && now % control_period == phase {
                let bytes = packet("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", "GS", now, vec![14]);
                let id = serialize::unpack_packet(&bytes).unwrap().packet_id();
                assert!(seen.insert(id));
                pending.insert(id, (now, 1));
                offered[1] += bytes.len() + 12 + 16;
                mesh.incoming(true, &encode(UP_DATA, &bytes), now);
            }
            if now <= 8000 && now % 1000 == 0 {
                let bytes = serialize::pack_packet(
                    &Packet::new(
                        sedsnet::config::DataType::DiscoveryAddress,
                        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                        "RF",
                        now,
                        Arc::from(metadata.clone()),
                    )
                    .unwrap(),
                )
                .to_vec();
                let id = serialize::unpack_packet(&bytes).unwrap().packet_id();
                assert!(seen.insert(id));
                pending.insert(id, (now, 2));
                offered[2] += bytes.len() + 28;
                mesh.incoming(false, &bytes, now);
            }
            mesh.tick(now);
            cap.apply_recovery_share(mesh.recovery_air_share(now), wall, Some(now));
            peak = peak.max(mesh.queue.len());
            assert!(
                mesh.queue.len() + usize::from(mesh.active_frame_count) <= MAX_FRAMES
                    && mesh.bytes <= MAX_BYTES
            );
            let mut count = 0;
            while count < 4 && wire.ready(now) {
                let Some(tx) =
                    mesh.begin_bounded_transmission(now, 0, !cap.ready(wall, Some(now)), 4 - count)
                else {
                    break;
                };
                if !cap.ready_for(wall, Some(now), tx.closes_packet_batch()) {
                    mesh.finish_bounded_transmission(tx, false);
                    break;
                }
                let mut lengths = [0; 2];
                for (i, frame) in [Some(tx.frame()), tx.follower_frame()]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(frame) = frame {
                        lengths[i] = frame.bytes.len() + 4;
                        charged += lengths[i] + 16;
                        if let Some((kind, body)) = decode(&frame.bytes)
                            && matches!(kind, DOWN_DATA | AV_BAY_DATA)
                            && let Ok(p) = serialize::unpack_packet(body)
                            && let Some((origin, control)) = pending.remove(&p.packet_id())
                        {
                            match control {
                                1 => control_ages.push(now - origin),
                                2 => metadata_ages.push(now - origin),
                                _ => {
                                    source_ages.push(now - origin);
                                    source_times.push(now)
                                }
                            }
                        }
                    }
                }
                let n = 1 + usize::from(lengths[1] > 0);
                pairs += usize::from(n == 2);
                wire.committed(now, lengths.iter().sum(), 115200);
                if n == 2 {
                    cap.committed_command_batch(wall, Some(now), lengths, lengths.iter().sum())
                } else {
                    cap.committed(wall, Some(now), lengths[0])
                }
                cap.drained_virtual(wire.stop_us());
                if tx.closes_packet_batch() {
                    let receipt = if let Some(contracts) = tx.batch_contracts() {
                        cap.close_control_batch(wall, Some(now), contracts).1
                    } else {
                        let c = tx.boundary_contract();
                        cap.close_packet_batch_for_contract(wall, Some(now), c)
                            && c.is_some_and(|(_, r)| r)
                    };
                    if receipt {
                        mesh.note_receipt_packet_boundary(cap.packet_idle_service_bytes())
                    }
                }
                cap.note_successful_ordinary_service(tx.successful_ordinary_service_bytes());
                mesh.finish_bounded_transmission(tx, true);
                count += n;
            }
        }
        let missing_source = pending.values().filter(|(_, c)| *c == 0).count();
        let missing_control = pending.values().filter(|(_, c)| *c == 1).count();
        let source_max = source_ages.iter().max().copied().unwrap_or(0);
        let control_max = control_ages.iter().max().copied().unwrap_or(0);
        let source_gap = source_times
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0);
        eprintln!(
            "THREE_LANE_BASELINE control_period_ms={control_period} source_period_ms=200 offered_source_charged={} offered_control_charged={} emitted_source={} emitted_control={} delivered_source={} delivered_control={} missing_source={missing_source} missing_control={missing_control} source_max_ms={source_max} source_max_gap_ms={source_gap} control_max_ms={control_max} source_late={} control_late={} actual_charged={charged} peak={peak} dropped={} pairs={pairs}",
            offered[0],
            offered[1],
            41,
            if phase == 0 {
                8000 / control_period + 1
            } else {
                8000 / control_period
            },
            source_ages.len(),
            control_ages.len(),
            source_ages.iter().filter(|&&a| a > 1500).count(),
            control_ages.iter().filter(|&&a| a > 500).count(),
            mesh.dropped
        );
        let missing_metadata = pending.values().filter(|(_, c)| *c == 2).count();
        let metadata_max = metadata_ages.iter().max().copied().unwrap_or(0);
        eprintln!(
            "THREE_LANE_METADATA control_period_ms={control_period} offered_metadata_charged={} delivered_metadata={} missing_metadata={missing_metadata} max_metadata_ms={metadata_max}",
            offered[2],
            metadata_ages.len()
        );
        assert_eq!(missing_metadata, 0, "metadata ownership");
        assert!(metadata_max <= 3000, "metadata age");
        assert_eq!(
            missing_source, 0,
            "under-capacitybaseline sourceownershiploss"
        );
        assert_eq!(missing_control, 0, "baseline controlownershiploss");
        assert!(source_max <= 1500, "baseline sourceage");
        assert!(control_max <= 500, "baseline controlage");
    }
}

#[test]
fn lower_band_critical_command_never_funds_ordinary_credit() {
    let mut mesh = Mesh::new(Role::Rrls, 2);
    let control = packet("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", "GS", 0, vec![14]);
    mesh.enqueue(
        Outgoing {
            command: true,
            bytes: encode(AV_BAY_DATA, &control),
        },
        200,
        0,
    );
    let tx = mesh.begin_bounded_transmission(0, 0, false, 2).unwrap();
    assert!(tx.closes_packet_batch());
    assert_eq!(tx.successful_ordinary_service_bytes(), 0);
    mesh.finish_bounded_transmission(tx, true);
}
