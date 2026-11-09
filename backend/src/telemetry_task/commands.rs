use super::prelude::*;
use super::{ROUTER_TX_BUDGET_MS, log_telemetry_error};

pub(super) fn log_command_dispatch(context: &str, side: &str, ty: DataType, payload: &[u8]) {
    let payload_preview = payload
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    log::info!(
        "command dispatched side={side} context=\"{context}\" ty={ty:?} payload={payload_preview}"
    );
}

/// Control completion depends on the controller, not on an unrelated SD logger.
/// Keep the station's local audit endpoint; controller-side state/event logging
/// remains independent of this command's end-to-end delivery obligation.
/// App-level provenance survives ordered retries; controllers suppress these
/// ordinary valve effects after launch while still accepting manual commands.
pub(crate) fn queue_automatic_controller_command(
    router: &Router,
    data_type: DataType,
    command: u8,
) -> sedsnet::TelemetryResult<()> {
    let ordinary_valve = if data_type == crate::telemetry_schema::data_type("VALVE_COMMAND") {
        matches!(command, 0..=5)
    } else if data_type == crate::telemetry_schema::data_type("ACTUATOR_COMMAND") {
        matches!(command, 9 | 10 | 12 | 13)
    } else {
        false
    };
    if !ordinary_valve {
        return Err(sedsnet::TelemetryError::Io(
            "unsupported automatic valve command",
        ));
    }
    queue_controller_command(router, data_type, &[command | 0x80])
}

pub(crate) fn controller_packet(
    router: &Router,
    data_type: DataType,
    payload: &[u8],
) -> sedsnet::TelemetryResult<Packet> {
    let owner = if data_type == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
        "FLIGHT_CONTROLLER"
    } else if data_type == crate::telemetry_schema::data_type("ACTUATOR_COMMAND") {
        "ACTUATOR_BOARD"
    } else if data_type == crate::telemetry_schema::data_type("VALVE_COMMAND") {
        "VALVE_BOARD"
    } else {
        return Err(sedsnet::TelemetryError::Io(
            "unsupported controller command",
        ));
    };
    Packet::new(
        data_type,
        &[
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            crate::telemetry_schema::endpoint(owner),
        ],
        &router.sender(),
        crate::system_clock::control_packet_timestamp_ms(router),
        Arc::from(payload),
    )
}

pub(crate) fn queue_controller_command(
    router: &Router,
    data_type: DataType,
    payload: &[u8],
) -> sedsnet::TelemetryResult<()> {
    router.tx_queue(controller_packet(router, data_type, payload)?)
}

pub(crate) fn queue_locally_routed_flight_command(
    state: &AppState,
    router: &Router,
    context: &str,
    payload: &[u8],
) -> sedsnet::TelemetryResult<()> {
    let revision = crate::board_abort::controller_can_launch(
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        payload,
    )
    .then(|| crate::board_abort::capture_launch_revision_named(state, "Launch"))
    .flatten();
    queue_locally_routed_flight_command_with_launch_revision(
        state, router, context, payload, revision,
    )
}
pub(crate) fn queue_locally_routed_flight_command_with_launch_revision(
    state: &AppState,
    router: &Router,
    context: &str,
    payload: &[u8],
    launch_revision: Option<u64>,
) -> sedsnet::TelemetryResult<()> {
    let topology = router.export_topology();
    let rocket_has_fc = topology.routes.iter().any(|route| {
        route.side_name == "rocket_comms"
            && route
                .reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"))
    });
    let umbilical_has_fc = topology.routes.iter().any(|route| {
        route.side_name == "umbilical_comms"
            && route
                .reachable_endpoints
                .contains(&crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"))
    });

    if !crate::gse::queue_controller_with_launch_revision(
        state,
        router,
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        payload,
        launch_revision,
    )? {
        return Err(sedsnet::TelemetryError::Io(
            "Manual Stop is pending; flight command was not queued",
        ));
    }

    log_command_dispatch(
        context,
        if rocket_has_fc && umbilical_has_fc {
            "rocket_comms,umbilical_comms"
        } else if rocket_has_fc {
            "rocket_comms"
        } else if umbilical_has_fc {
            "umbilical_comms"
        } else {
            "broadcast"
        },
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        payload,
    );
    Ok(())
}

pub(crate) const MANUAL_ABORT_PREFIX: &str = "SEDS_MANUAL_ABORT_V1:";

/// SDK owns the marked manual Abort before local bookkeeping runs.
/// The actual logical router sender also owns its fallback ACK return path.
#[cfg(test)]
pub(crate) fn queue_priority_manual_abort_packet(router: &Router) -> sedsnet::TelemetryResult<()> {
    queue_manual_abort_wire(router, crate::manual_abort_intent::WirePolicy::Untimed)
}

#[cfg(test)]
pub(crate) fn queue_manual_abort_wire(
    router: &Router,
    wire: crate::manual_abort_intent::WirePolicy,
) -> sedsnet::TelemetryResult<()> {
    queue_manual_abort_wire_receipted(router, wire).map(|_| ())
}

/// Receipt metadata exists only after the actual original entered SDK ownership.
pub(crate) fn queue_manual_abort_wire_receipted(
    router: &Router,
    wire: crate::manual_abort_intent::WirePolicy,
) -> sedsnet::TelemetryResult<(u64, String, u64)> {
    let (body, original) = match wire {
        crate::manual_abort_intent::WirePolicy::Untimed => (MANUAL_ABORT_PREFIX, 0),
        crate::manual_abort_intent::WirePolicy::KnownUtc(original) => {
            ("SEDS_MANUAL_ABORT_V1:K", original)
        }
    };
    let packet = abort_packet_from_at(router, body, &router.sender(), original)?;
    let receipt = (packet.packet_id(), packet.sender().to_string(), packet.timestamp());
    router.tx_queue(packet)?;
    Ok(receipt)
}

pub(crate) fn queue_abort_packet(router: &Router, reason: &str) -> sedsnet::TelemetryResult<()> {
    router.rx_queue(abort_packet(router, reason)?)
}

/// Locally generated automatic aborts retain accepted TX ownership until handoff.
pub(crate) fn queue_automatic_abort_packet(
    router: &Router,
    reason: &str,
    cause: crate::abort_retirement::AbortCause,
) -> sedsnet::TelemetryResult<()> {
    let marked = format!("SEDS_AUTO_ABORT_V1:{reason}");
    let packet = abort_packet(router, &marked)?;
    let packet_id = packet.packet_id();
    router.tx_queue(packet)?;
    // Only automatic aborts are registered, so only they can be retired.
    crate::abort_retirement::register_automatic(router, packet_id, reason, cause);
    Ok(())
}

fn abort_packet(router: &Router, reason: &str) -> sedsnet::TelemetryResult<Packet> {
    abort_packet_from(router, reason, Board::GroundStation.sender_id())
}

fn abort_packet_from(
    router: &Router,
    reason: &str,
    sender: &str,
) -> sedsnet::TelemetryResult<Packet> {
    abort_packet_from_at(
        router,
        reason,
        sender,
        crate::system_clock::control_packet_timestamp_ms(router),
    )
}

fn abort_packet_from_at(
    _router: &Router,
    reason: &str,
    sender: &str,
    original: u64,
) -> sedsnet::TelemetryResult<Packet> {
    Packet::new(
        crate::telemetry_schema::data_type("ABORT"),
        &[
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            crate::telemetry_schema::endpoint("VALVE_BOARD"),
            crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
            crate::telemetry_schema::endpoint("ABORT"),
            crate::telemetry_schema::endpoint("FLIGHT_STATE"),
            crate::telemetry_schema::endpoint("SD_CARD"),
        ],
        sender,
        original,
        Arc::from(reason.as_bytes()),
    )
}

#[cfg(test)]
mod control_clock_tests {
    use super::*;

    #[test]
    fn manual_and_marked_abort_headers_use_the_same_network_clock_as_controllers() {
        let router = Router::new_with_clock(
            sedsnet::router::RouterConfig::new([]).with_sender("GS"),
            Box::new(|| 0),
        );
        let network_utc = 1_800_000_000_123;
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                network_utc,
            ))
            .unwrap();
        for reason in [
            "Manual Abort Command Issued",
            "SEDS_AUTO_ABORT_V1:pressure ceiling",
        ] {
            let packet = abort_packet(&router, reason).unwrap();
            assert_eq!(packet.timestamp(), network_utc);
            assert_eq!(packet.payload(), reason.as_bytes());
        }
    }

    #[test]
    fn manual_abort_remains_constructible_before_network_utc_is_available() {
        let router = Router::new_with_clock(sedsnet::router::RouterConfig::new([]), Box::new(|| 0));
        let before = crate::telemetry_task::get_current_timestamp_ms();
        let packet = abort_packet(&router, "Manual Abort Command Issued").unwrap();
        let after = crate::telemetry_task::get_current_timestamp_ms();
        assert!((before..=after).contains(&packet.timestamp()));
        assert!(crate::system_clock::required_control_network_utc_ms(&router).is_err());
    }
}

pub(crate) fn flush_command_tx(router: &Router, context: &str) {
    if let Err(err) = router.process_all_queues_with_timeout(ROUTER_TX_BUDGET_MS) {
        log_telemetry_error(context, err);
    }
}

#[cfg(test)]
mod destination_latency_tests {
    use super::*;
    use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};

    #[tokio::test]
    async fn flight_actions_do_not_wait_for_an_unresponsive_recording_endpoint() {
        let state = crate::state::tests::test_app_state().await;
        let now = Arc::new(AtomicU64::new(0));
        let clock = now.clone();
        let audited = Arc::new(Mutex::new(Vec::new()));
        let local_audit = audited.clone();
        let station = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                        local_audit.lock().unwrap().push(packet.payload()[0]);
                    }
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || clock.load(Ordering::Relaxed)),
        );
        let to_flight = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let outgoing = to_flight.clone();
        let station_side = station.add_side_packed_with_options(
            "rocket_comms",
            move |packet| {
                outgoing.lock().unwrap().push(packet.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        let received = Arc::new(Mutex::new(Vec::new()));
        let actions = received.clone();
        let clock = now.clone();
        let action_clock = now.clone();
        let flight = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                move |packet| {
                    if packet.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                        actions
                            .lock()
                            .unwrap()
                            .push((packet.payload()[0], action_clock.load(Ordering::Relaxed)));
                    }
                    Ok(())
                },
            )])
            .with_sender("FC"),
            Box::new(move || clock.load(Ordering::Relaxed)),
        );
        let to_station = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let returning = to_station.clone();
        let flight_side = flight.add_side_packed_with_options(
            "can",
            move |packet| {
                returning.lock().unwrap().push(packet.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        for (sender, endpoints) in [
            (
                "FC",
                vec![crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            ),
            ("DAQ", vec![crate::telemetry_schema::endpoint("SD_CARD")]),
        ] {
            station
                .rx_from_side(
                    &sedsnet::discovery::build_discovery_announce(sender, 1, &endpoints).unwrap(),
                    station_side,
                )
                .unwrap();
        }
        flight
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    1,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .unwrap(),
                flight_side,
            )
            .unwrap();
        queue_locally_routed_flight_command(&state, &station, "first fixture action", &[14])
            .unwrap();
        for at in (0..=1000).step_by(10) {
            now.store(at, Ordering::Relaxed);
            if at == 100 {
                queue_locally_routed_flight_command(
                    &state,
                    &station,
                    "synthetic deploy request",
                    &[12],
                )
                .unwrap();
            }
            for _ in 0..4 {
                station.process_all_queues_with_timeout(0).unwrap();
                let packets = std::mem::take(&mut *to_flight.lock().unwrap());
                for packet in packets {
                    flight.rx_packed_from_side(&packet, flight_side).unwrap();
                }
                flight.process_all_queues_with_timeout(0).unwrap();
                let packets = std::mem::take(&mut *to_station.lock().unwrap());
                for packet in packets {
                    station.rx_packed_from_side(&packet, station_side).unwrap();
                }
            }
        }
        assert_eq!(
            *audited.lock().unwrap(),
            [14, 12],
            "controller-targeted commands retain the local GS audit"
        );
        let received = received.lock().unwrap();
        assert!(
            received.iter().any(|&(command, _)| command == 14),
            "first action must reach the live FC: {received:?}"
        );
        assert!(
            received
                .iter()
                .any(|&(command, at)| command == 12 && at <= 600),
            "a separate unresponsive SD owner must not hold an FC action behind its predecessor: {received:?}"
        );
    }
}

#[cfg(test)]
mod automatic_provenance_tests {
    use super::*;
    #[test]
    fn only_ordinary_valves_can_have_automatic_wire_provenance() {
        crate::telemetry_schema::initialize().unwrap();
        for (kind, allowed) in [
            ("ACTUATOR_COMMAND", &[9u8, 10, 12, 13][..]),
            ("VALVE_COMMAND", &[0u8, 1, 2, 3, 4, 5][..]),
            ("FLIGHT_COMMAND", &[][..]),
        ] {
            for command in 0u8..=127 {
                let router = Router::new(sedsnet::router::RouterConfig::new([]));
                let result = queue_automatic_controller_command(
                    &router,
                    crate::telemetry_schema::data_type(kind),
                    command,
                );
                assert_eq!(
                    result.is_ok(),
                    allowed.contains(&command),
                    "{kind}/{command}"
                );
            }
        }
    }
}

#[cfg(test)]
mod manual_stop_owner_tests {
    use super::*;
    use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};

    #[test]
    fn manual_abort_actual_sdk_full_queue_refusal_retains_exact_known_timestamp_and_payload() {
        use crate::manual_abort_intent::{ManualAbortIntent, WirePolicy};
        crate::telemetry_schema::initialize().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let output = captured.clone();
        let router = Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    output
                        .lock()
                        .unwrap()
                        .push((packet.timestamp(), packet.payload().to_vec()));
                    Ok(())
                },
            )])
            .with_sender("GS")
            .with_memory_config(
                sedsnet::config::RuntimeMemoryConfig::new(65536, 8, 2048, 2.0).unwrap(),
            )
            .unwrap(),
            Box::new(|| 0),
        );
        let mut filled = 0;
        while queue_manual_abort_wire(&router, WirePolicy::Untimed).is_ok() {
            filled += 1;
            assert!(filled < 1024, "bounded real queue must refuse");
        }
        assert!(filled > 0);
        let intent = Arc::new(ManualAbortIntent::default());
        intent.request();
        let mut claim = intent.try_claim().unwrap();
        let original = 1_793_000_000_123;
        let wire = claim.freeze_wire(WirePolicy::KnownUtc(original)).unwrap();
        assert!(queue_manual_abort_wire(&router, wire).is_err());
        assert_eq!(intent.try_known_highwater(), Some(original));
        drop(claim);
        router.clear_tx_queue();
        router
            .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
                original + 10_000,
            ))
            .unwrap();
        let mut retry = intent.try_claim().unwrap();
        let wire = retry
            .freeze_wire(WirePolicy::KnownUtc(original + 10_000))
            .unwrap();
        assert_eq!(wire, WirePolicy::KnownUtc(original));
        queue_manual_abort_wire(&router, wire).unwrap();
        retry.accepted();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(
            &*captured.lock().unwrap(),
            &[(original, b"SEDS_MANUAL_ABORT_V1:K".to_vec())]
        );
        assert_eq!(intent.pending_epoch(), None);
        queue_manual_abort_wire(&router, WirePolicy::Untimed).unwrap();
        router.process_all_queues_with_timeout(0).unwrap();
        assert_eq!(
            captured.lock().unwrap().last().unwrap(),
            &(0, b"SEDS_MANUAL_ABORT_V1:".to_vec())
        );
    }

    #[test]
    fn marked_manual_stop_retires_distinct_owners_for_primary_and_rrls_senders() {
        crate::telemetry_schema::initialize().unwrap();
        assert_eq!(MANUAL_ABORT_PREFIX.len(), 21);
        let abort = crate::telemetry_schema::data_type("ABORT");
        let endpoints = [
            "GROUND_STATION",
            "FLIGHT_CONTROLLER",
            "VALVE_BOARD",
            "ACTUATOR_BOARD",
            "ABORT",
            "FLIGHT_STATE",
            "SD_CARD",
        ]
        .map(crate::telemetry_schema::endpoint);
        for station_name in ["GS", "RRLS"] {
            let now = Arc::new(AtomicU64::new(0));
            let received = Arc::new(Mutex::new(HashMap::<String, Vec<u64>>::new()));
            let station_out = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let to_station = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let to_can = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let from_can = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let make = |name: &str, owners: &[DataEndpoint]| {
                let handlers = owners
                    .iter()
                    .map(|&endpoint| {
                        let received = received.clone();
                        let name = name.to_string();
                        EndpointHandler::new_packet_handler(endpoint, move |packet| {
                            if packet.data_type() == abort {
                                assert_eq!(packet.payload(), MANUAL_ABORT_PREFIX.as_bytes());
                                assert_eq!(
                                    packet.timestamp(),
                                    0,
                                    "legacy remains untimed at the actual peer Abort owner"
                                );
                                let mut rows = received.lock().unwrap();
                                let ids = rows.entry(name.clone()).or_default();
                                if !ids.contains(&packet.packet_id()) {
                                    ids.push(packet.packet_id());
                                }
                            }
                            Ok(())
                        })
                    })
                    .collect::<Vec<_>>();
                let clock = now.clone();
                let router = Router::new_with_clock(
                    RouterConfig::new(handlers).with_sender(name),
                    Box::new(move || clock.load(Ordering::Relaxed)),
                );
                router
                    .try_set_local_network_time(
                        sedsnet::timesync::PartialNetworkTime::from_unix_ms(1_800_000_000_123),
                    )
                    .unwrap();
                router
            };
            let station = make(station_name, &[endpoints[0], endpoints[4], endpoints[5]]);
            let rf = make("RF", &[]);
            let options = || RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            };
            let output = station_out.clone();
            let station_side = station.add_side_packed_with_options(
                "radio",
                move |bytes| {
                    output.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                },
                options(),
            );
            let output = to_station.clone();
            let rf_radio = rf.add_side_packed_with_options(
                "radio",
                move |bytes| {
                    output.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                },
                options(),
            );
            let output = to_can.clone();
            let rf_can = rf.add_side_packed_with_options(
                "can",
                move |bytes| {
                    output.lock().unwrap().push(bytes.to_vec());
                    Ok(())
                },
                options(),
            );
            let mut nodes = Vec::new();
            for (name, owned) in [
                ("FC", vec![endpoints[1], endpoints[4]]),
                ("VB", vec![endpoints[2], endpoints[4]]),
                ("AB", vec![endpoints[3], endpoints[4]]),
                ("DAQ", vec![endpoints[6], endpoints[5]]),
            ] {
                let node = make(name, &owned);
                let output = from_can.clone();
                let side = node.add_side_packed_with_options(
                    "can",
                    move |bytes| {
                        output.lock().unwrap().push(bytes.to_vec());
                        Ok(())
                    },
                    options(),
                );
                let announcement =
                    sedsnet::discovery::build_discovery_announce(name, 1, &owned).unwrap();
                station.rx_from_side(&announcement, station_side).unwrap();
                rf.rx_from_side(&announcement, rf_can).unwrap();
                let announcement = sedsnet::discovery::build_discovery_announce(
                    station_name,
                    1,
                    &[endpoints[0], endpoints[4], endpoints[5]],
                )
                .unwrap();
                node.rx_from_side(&announcement, side).unwrap();
                nodes.push((node, side));
            }
            rf.rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    station_name,
                    1,
                    &[endpoints[0], endpoints[4], endpoints[5]],
                )
                .unwrap(),
                rf_radio,
            )
            .unwrap();
            let intent = Arc::new(crate::manual_abort_intent::ManualAbortIntent::default());
            let mut epochs_received = Vec::new();
            for epoch in 1..=2 {
                assert_eq!(intent.request(), epoch);
                now.store((epoch - 1) * 1000, Ordering::Relaxed);
                assert!(station.network_time_ms().unwrap() >= 1_800_000_000_123);
                let claim = intent.try_claim().unwrap();
                queue_priority_manual_abort_packet(&station).unwrap();
                claim.accepted();
                assert!(station.has_pending_reliable_tx_type(abort));
                let mut transmitted = Vec::new();
                for at in (((epoch - 1) * 1000)..=(epoch * 1000)).step_by(10) {
                    now.store(at, Ordering::Relaxed);
                    for _ in 0..8 {
                        station.process_all_queues_with_timeout(0).unwrap();
                        for wire in std::mem::take(&mut *station_out.lock().unwrap()) {
                            if let Ok(packet) = serialize::unpack_packet(&wire)
                                && packet.data_type() == abort {
                                    transmitted.push(packet);
                                }
                            rf.rx_packed_from_side(&wire, rf_radio).unwrap();
                        }
                        rf.process_all_queues_with_timeout(0).unwrap();
                        for wire in std::mem::take(&mut *to_can.lock().unwrap()) {
                            for (node, side) in &nodes {
                                node.rx_packed_from_side(&wire, *side).unwrap();
                            }
                        }
                        for (node, _) in &nodes {
                            node.process_all_queues_with_timeout(0).unwrap();
                        }
                        for wire in std::mem::take(&mut *from_can.lock().unwrap()) {
                            rf.rx_packed_from_side(&wire, rf_can).unwrap();
                        }
                        rf.process_all_queues_with_timeout(0).unwrap();
                        for wire in std::mem::take(&mut *to_station.lock().unwrap()) {
                            station.rx_packed_from_side(&wire, station_side).unwrap();
                        }
                    }
                }
                assert!(!transmitted.is_empty());
                let packet_id = transmitted[0].packet_id();
                assert!(
                    !epochs_received.contains(&packet_id),
                    "a new manual epoch must get a distinct SDK envelope"
                );
                epochs_received.push(packet_id);
                for packet in &transmitted {
                    assert_eq!(packet.sender(), station_name);
                    assert_eq!(packet.packet_id(), packet_id);
                    assert_eq!(packet.payload(), MANUAL_ABORT_PREFIX.as_bytes());
                    assert_eq!(packet.timestamp(), 0);
                    assert_eq!(packet.endpoints().len(), endpoints.len());
                    for endpoint in endpoints {
                        assert!(packet.endpoints().contains(&endpoint));
                    }
                }
                let rows = received.lock().unwrap();
                for owner in [station_name, "FC", "AB", "VB", "DAQ"] {
                    assert_eq!(
                        rows.get(owner),
                        Some(&epochs_received),
                        "{station_name}: {owner}"
                    );
                }
                assert!(
                    !station.has_pending_reliable_tx_type(abort),
                    "ACKs must retire the {station_name} sender's SDK owner"
                );
                assert_eq!(intent.pending_epoch(), None);
            }
        }
    }
}
