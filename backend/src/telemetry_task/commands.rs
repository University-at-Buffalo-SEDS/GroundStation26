use super::prelude::*;
use super::{ROUTER_TX_BUDGET_MS, get_current_timestamp_ms, log_telemetry_error};

static FLIGHT_COMMAND_TX_SIDES: OnceLock<Mutex<Vec<FlightCommandTxSide>>> = OnceLock::new();

#[derive(Clone)]
struct FlightCommandTxSide {
    name: &'static str,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

pub(crate) fn register_flight_command_tx_side(
    name: &'static str,
    tx: mpsc::UnboundedSender<Vec<u8>>,
) {
    let sides = FLIGHT_COMMAND_TX_SIDES.get_or_init(|| Mutex::new(Vec::new()));
    let mut sides = sides
        .lock()
        .expect("failed to lock flight command tx sides");
    if let Some(side) = sides.iter_mut().find(|side| side.name == name) {
        side.tx = tx;
        return;
    }
    sides.push(FlightCommandTxSide { name, tx });
}

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

fn build_flight_command_packet(payload: &[u8]) -> sedsprintf_rs_2026::TelemetryResult<Packet> {
    Packet::new(
        DataType::FlightCommand,
        &[DataEndpoint::FlightController],
        Board::GroundStation.sender_id(),
        get_current_timestamp_ms(),
        Arc::from(payload),
    )
}

pub(super) fn queue_locally_routed_flight_command(
    router: &Router,
    context: &str,
    payload: &[u8],
) -> sedsprintf_rs_2026::TelemetryResult<()> {
    let topology = router.export_topology();
    let rocket_has_fc = topology.routes.iter().any(|route| {
        route.side_name == "rocket_comms"
            && route
                .reachable_endpoints
                .contains(&DataEndpoint::FlightController)
    });
    let umbilical_has_fc = topology.routes.iter().any(|route| {
        route.side_name == "umbilical_comms"
            && route
                .reachable_endpoints
                .contains(&DataEndpoint::FlightController)
    });

    let direct_radio_sent = if let Some(sides) = FLIGHT_COMMAND_TX_SIDES.get() {
        let pkt = build_flight_command_packet(payload)?;
        let wire = serialize::serialize_packet(&pkt);
        let sides = sides
            .lock()
            .expect("failed to lock flight command tx sides");
        let mut sent_any = false;
        for side in sides.iter().filter(|side| side.name == "rocket_comms") {
            if side.tx.send(wire.to_vec()).is_ok() {
                sent_any = true;
            }
        }
        sent_any
    } else {
        false
    };

    if !direct_radio_sent {
        router.log_queue(DataType::FlightCommand, payload)?;
    }

    log_command_dispatch(
        context,
        if direct_radio_sent {
            "rocket_comms"
        } else if rocket_has_fc && umbilical_has_fc {
            "rocket_comms,umbilical_comms"
        } else if rocket_has_fc {
            "rocket_comms"
        } else if umbilical_has_fc {
            "umbilical_comms"
        } else {
            "broadcast"
        },
        DataType::FlightCommand,
        payload,
    );
    Ok(())
}

pub(crate) fn queue_abort_packet(
    router: &Router,
    reason: &str,
) -> sedsprintf_rs_2026::TelemetryResult<()> {
    let pkt = Packet::new(
        DataType::Abort,
        &[
            DataEndpoint::GroundStation,
            DataEndpoint::FlightController,
            DataEndpoint::ValveBoard,
            DataEndpoint::ActuatorBoard,
            DataEndpoint::Abort,
            DataEndpoint::FlightState,
            DataEndpoint::SdCard,
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
