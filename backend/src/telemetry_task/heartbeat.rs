//! Preserve the configured heartbeat recipients and reach each fill watchdog.
use super::prelude::*;

fn station_heartbeat_packet(
    router: &Router,
    timestamp_ms: u64,
) -> sedsnet::TelemetryResult<Packet> {
    let ty = crate::telemetry_schema::data_type("HEARTBEAT");
    let mut endpoints = sedsnet::endpoints_from_datatype(ty).to_vec();
    let sender = router.sender();
    if sender.as_ref() == Board::GroundStation.sender_id() {
        // HEART_BEAT is shared by several boards. Once discovery converges,
        // one physical branch can satisfy that endpoint and exclude the fill
        // watchdogs. Their unique control endpoints keep both routes selected.
        for name in ["ACTUATOR_BOARD", "VALVE_BOARD"] {
            let endpoint = crate::telemetry_schema::endpoint(name);
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        }
    }
    Packet::new(ty, &endpoints, &sender, timestamp_ms, Arc::from([]))
}

pub(super) fn queue_station_heartbeat(router: &Router) -> sedsnet::TelemetryResult<()> {
    let timestamp_ms = router
        .network_time_ms()
        .unwrap_or_else(super::get_current_timestamp_ms);
    router.tx_queue(station_heartbeat_packet(router, timestamp_ms)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn only_the_primary_adds_watchdog_routes_and_preserves_configured_recipients() {
        let ty = crate::telemetry_schema::data_type("HEARTBEAT");
        let defaults = sedsnet::endpoints_from_datatype(ty);
        for sender in ["GS", "RRLS", "GB"] {
            let router = Router::new(RouterConfig::new([]).with_sender(sender));
            let packet = station_heartbeat_packet(&router, 123).unwrap();
            assert_eq!(packet.sender(), sender);
            assert_eq!(packet.timestamp(), 123);
            assert!(packet.payload().is_empty());
            assert!(defaults.iter().all(|ep| packet.endpoints().contains(ep)));
            let expected = defaults.len() + if sender == "GS" { 2 } else { 0 };
            assert_eq!(packet.endpoints().len(), expected);
        }
    }

    #[derive(Clone)]
    struct Frame {
        to: usize,
        side: usize,
        bytes: Vec<u8>,
    }

    #[test]
    fn explicit_watchdog_routes_reach_both_fill_boards_after_discovery_converges() {
        let ty = crate::telemetry_schema::data_type("HEARTBEAT");
        let ep = |name| crate::telemetry_schema::endpoint(name);
        let hb = ep("HEART_BEAT");
        let now = Arc::new(AtomicU64::new(0));
        let frames = Arc::new(Mutex::new(Vec::<Frame>::new()));
        let hits = Arc::new(Mutex::new(Vec::<(u64, usize)>::new()));
        let names = ["GS", "RF", "PB", "FC", "GB", "AB", "VB", "DAQ"];
        let mut nodes = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let endpoints = match index {
                0 => vec![ep("GROUND_STATION")],
                2 => vec![hb],
                3 => vec![ep("FLIGHT_CONTROLLER")],
                5 => vec![ep("ACTUATOR_BOARD"), hb],
                6 => vec![ep("VALVE_BOARD"), hb],
                _ => vec![],
            };
            let handlers = endpoints
                .into_iter()
                .map(|endpoint| {
                    let hits = hits.clone();
                    let clock = now.clone();
                    EndpointHandler::new_packet_handler(endpoint, move |packet| {
                        if packet.data_type() == ty && packet.sender() == "GS" {
                            hits.lock()
                                .unwrap()
                                .push((clock.load(Ordering::Relaxed), index));
                        }
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            let clock = now.clone();
            nodes.push(Router::new_with_clock(
                RouterConfig::new(handlers).with_sender(*name),
                Box::new(move || clock.load(Ordering::Relaxed)),
            ));
        }
        // Exact physical branches: GS-RF-rocket CAN, GS-GB-fill CAN.
        // One callback per physical CAN side broadcasts to its connected nodes.
        let sides = [
            (0, vec![(1, 0)]),
            (0, vec![(4, 0)]),
            (1, vec![(0, 0)]),
            (1, vec![(2, 0), (3, 0)]),
            (2, vec![(1, 1), (3, 0)]),
            (3, vec![(1, 1), (2, 0)]),
            (4, vec![(0, 1)]),
            (4, vec![(5, 0), (6, 0), (7, 0)]),
            (5, vec![(4, 1), (6, 0), (7, 0)]),
            (6, vec![(4, 1), (5, 0), (7, 0)]),
            (7, vec![(4, 1), (5, 0), (6, 0)]),
        ];
        for (from, targets) in sides {
            let queue = frames.clone();
            nodes[from].add_side_packed_with_priority_and_options(
                "heartbeat regression",
                move |bytes, _| {
                    for &(to, side) in &targets {
                        queue.lock().unwrap().push(Frame {
                            to,
                            side,
                            bytes: bytes.to_vec(),
                        });
                    }
                    Ok(())
                },
                RouterSideOptions {
                    reliable_enabled: false,
                    header_template_enabled: false,
                    ..Default::default()
                },
            );
        }
        for node in &nodes {
            node.announce_discovery().unwrap();
        }
        for tick in (0..=12_000).step_by(10) {
            now.store(tick, Ordering::Relaxed);
            if tick % 500 == 0 {
                if tick < 6_000 {
                    nodes[0].log_queue::<u8>(ty, &[]).unwrap();
                } else {
                    queue_station_heartbeat(&nodes[0]).unwrap();
                }
            }
            for node in &nodes {
                node.poll_discovery().unwrap();
                node.process_all_queues().unwrap();
            }
            for _ in 0..20 {
                let due = std::mem::take(&mut *frames.lock().unwrap());
                if due.is_empty() {
                    break;
                }
                for frame in due {
                    nodes[frame.to]
                        .rx_packed_queue_from_side(&frame.bytes, frame.side)
                        .unwrap();
                }
                for node in &nodes {
                    node.process_all_queues().unwrap();
                }
            }
        }
        let hits = hits.lock().unwrap();
        for board in [5, 6] {
            assert!(
                !hits.iter().any(|&(at, node)| at < 6_000 && node == board),
                "legacy default recipient policy unexpectedly reached {}",
                names[board]
            );
            for at in (6_000..=12_000).step_by(500) {
                assert!(
                    hits.contains(&(at, board)),
                    "{} missed genuine GS heartbeat at {at}",
                    names[board]
                );
            }
        }
        assert!(
            hits.iter().any(|&(at, node)| at >= 6_000 && node == 2),
            "existing rocket HEART_BEAT recipient lost"
        );
        assert!(
            hits.iter().any(|&(at, node)| at >= 6_000 && node == 3),
            "existing flight-controller recipient lost"
        );
    }
}
