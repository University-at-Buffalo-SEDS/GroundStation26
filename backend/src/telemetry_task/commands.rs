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

pub(super) fn queue_locally_routed_flight_command(
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

    router.log_queue(
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
    let pkt = Packet::new(
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
    )?;
    router.rx_queue(pkt)
}

pub(super) fn flush_command_tx(router: &Router, context: &str) {
    if let Err(err) = router.process_all_queues_with_timeout(ROUTER_TX_BUDGET_MS) {
        log_telemetry_error(context, err);
    }
}
