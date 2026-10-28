use super::prelude::*;
use super::{ROUTER_TX_BUDGET_MS, get_current_timestamp_ms, log_telemetry_error};

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
pub(crate) fn queue_controller_command(
    router: &Router,
    data_type: DataType,
    payload: &[u8],
) -> sedsnet::TelemetryResult<()> {
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
    router.tx_queue(Packet::new(
        data_type,
        &[
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            crate::telemetry_schema::endpoint(owner),
        ],
        &router.sender(),
        get_current_timestamp_ms(),
        Arc::from(payload),
    )?)
}

pub(crate) fn queue_locally_routed_flight_command(
    router: &Router,
    context: &str,
    payload: &[u8],
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

    queue_controller_command(
        router,
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        payload,
    )?;

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

pub(crate) fn queue_abort_packet(router: &Router, reason: &str) -> sedsnet::TelemetryResult<()> {
    router.rx_queue(abort_packet(reason)?)
}

/// Locally generated automatic aborts retain accepted TX ownership until handoff.
pub(crate) fn queue_automatic_abort_packet(
    router: &Router,
    reason: &str,
) -> sedsnet::TelemetryResult<()> {
    router.tx_queue(abort_packet(reason)?)
}

fn abort_packet(reason: &str) -> sedsnet::TelemetryResult<Packet> {
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
        Board::GroundStation.sender_id(),
        get_current_timestamp_ms(),
        Arc::from(reason.as_bytes()),
    )
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

    #[test]
    fn flight_actions_do_not_wait_for_an_unresponsive_recording_endpoint() {
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
        queue_locally_routed_flight_command(&station, "first fixture action", &[14]).unwrap();
        for at in (0..=1000).step_by(10) {
            now.store(at, Ordering::Relaxed);
            if at == 100 {
                queue_locally_routed_flight_command(&station, "synthetic deploy request", &[12])
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
