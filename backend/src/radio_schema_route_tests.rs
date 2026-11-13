//! Discovery schema exchange stays off the radio (`keep_schema_exchange_off_the_radio`).
use sedsnet::config::DataType;
use sedsnet::packet::Packet;
use sedsnet::router::{EndpointHandler, NetworkVariablePermissions, Router, RouterConfig, RouterSideId};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type Wire = Arc<Mutex<Vec<Vec<u8>>>>;

struct Bay {
    gs: Arc<Router>,
    rf: Arc<Router>,
    gb: Arc<Router>,
    radio: RouterSideId,
    umbilical: RouterSideId,
    wires: [(Wire, Wire, RouterSideId); 2],
    now: Arc<AtomicU64>,
}

fn wire_pair(host: &Arc<Router>, name: &str, peer: &Arc<Router>, opts: sedsnet::router::RouterSideOptions) -> (RouterSideId, Wire, Wire, RouterSideId) {
    let out: Wire = Default::default();
    let back: Wire = Default::default();
    let o = out.clone();
    let side = host.add_side_packed_with_options(name, move |b| { o.lock().unwrap().push(b.to_vec()); Ok(()) }, opts.clone());
    let r = back.clone();
    let peer_side = peer.add_side_packed_with_options("GS", move |b| { r.lock().unwrap().push(b.to_vec()); Ok(()) }, opts);
    (side, out, back, peer_side)
}

fn bay(gs_received: Arc<AtomicUsize>) -> Bay {
    crate::telemetry_schema::initialize().unwrap();
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = |n: &Arc<AtomicU64>| { let n = n.clone(); Box::new(move || n.load(Ordering::Relaxed)) as Box<dyn sedsnet::router::Clock + Send + Sync> };
    let seen = gs_received;
    let gs = Arc::new(Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint("GROUND_STATION"), move |_| {
            seen.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })])
        .with_sender("GS"),
        clock(&now),
    ));
    let rf = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("RF"), clock(&now)));
    let gb = Arc::new(Router::new_with_clock(RouterConfig::new([]).with_sender("GB"), clock(&now)));
    let link: crate::comms_config::CommsLinkConfig =
        serde_json::from_value(serde_json::json!({"interface": "serial", "port": "/dev/test-radio", "protocol": "raw_uart"})).unwrap();
    let (radio, r_out, r_back, rf_side) = wire_pair(&gs, "rocket_comms", &rf, crate::rocket_router_side_options(&link, false));
    let (umbilical, u_out, u_back, gb_side) =
        wire_pair(&gs, "umbilical_comms", &gb, sedsnet::router::RouterSideOptions::default().with_small_packet_transport(1024));
    Bay { gs, rf, gb, radio, umbilical, wires: [(r_out, r_back, rf_side), (u_out, u_back, gb_side)], now }
}

impl Bay {
    fn pump(&self, steps: usize) {
        for _ in 0..steps {
            self.now.fetch_add(50, Ordering::Relaxed);
            for r in [&self.gs, &self.rf, &self.gb] {
                let _ = r.poll_discovery();
                let _ = r.process_all_queues();
            }
            for (i, (out, back, peer_side)) in self.wires.iter().enumerate() {
                let (peer, host_side) = if i == 0 { (&self.rf, self.radio) } else { (&self.gb, self.umbilical) };
                for b in std::mem::take(&mut *out.lock().unwrap()) {
                    let _ = peer.rx_packed_from_side(&b, *peer_side);
                }
                for b in std::mem::take(&mut *back.lock().unwrap()) {
                    let _ = self.gs.rx_packed_from_side(&b, host_side);
                }
            }
        }
    }
    fn tx(&self, side: RouterSideId, ty: DataType) -> u64 {
        self.gs
            .export_runtime_stats()
            .sides
            .iter()
            .filter(|s| s.side_id == side)
            .flat_map(|s| s.data_types.iter())
            .filter(|k| k.data_type == ty)
            .map(|k| k.tx_packets)
            .sum()
    }
}

/// Runs discovery for 60 s with both peers announcing and asking for the
/// station's schema. Returns (radio schema, radio schema requests, umbilical
/// schema) packets the station sent, and whether RF data still reached it.
fn exchange(deny: bool) -> (u64, u64, u64, bool) {
    let received = Arc::new(AtomicUsize::new(0));
    let bay = bay(received.clone());
    if deny {
        crate::keep_schema_exchange_off_the_radio(&bay.gs, bay.radio, &[bay.umbilical]).unwrap();
    }
    bay.gs.announce_discovery().unwrap();
    bay.rf.announce_discovery().unwrap();
    bay.gb.announce_discovery().unwrap();
    bay.pump(40);
    for _ in 0..6 {
        bay.rf.request_schema().unwrap();
        bay.gb.request_schema().unwrap();
        bay.gs.request_schema().unwrap();
        bay.pump(200);
    }
    // Ordinary AV-bay telemetry still reaches the station over the radio.
    let before = received.load(Ordering::Relaxed);
    bay.rf
        .tx_queue(
            Packet::new(
                crate::telemetry_schema::data_type("BATTERY_VOLTAGE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "RF",
                bay.now.load(Ordering::Relaxed),
                Arc::from(1.5f32.to_le_bytes()),
            )
            .unwrap(),
        )
        .unwrap();
    bay.pump(40);
    (
        bay.tx(bay.radio, DataType::DiscoverySchema),
        bay.tx(bay.radio, DataType::DiscoverySchemaRequest),
        bay.tx(bay.umbilical, DataType::DiscoverySchema),
        received.load(Ordering::Relaxed) > before,
    )
}

/// The station sent a 26-chunk schema and schema requests over the radio and
/// RF answered with its own 20-chunk schema; under radio loss none completed
/// and both ends kept asking. The radio side carries neither, whoever asks
/// and whichever side the schema came from; the umbilical still exchanges
/// schema and radio data still flows.
#[test]
fn schema_exchange_stays_off_the_radio_and_flows_on_the_umbilical() {
    let (open_schema, open_requests, _, _) = exchange(false);
    assert!(open_schema > 0 && open_requests > 0, "without the deny the radio carries schema traffic ({open_schema}, {open_requests})");
    let (schema, requests, umbilical, data) = exchange(true);
    eprintln!("RADIO_SCHEMA open=({open_schema},{open_requests}) denied=({schema},{requests}) umbilical_schema={umbilical} rf_data={data}");
    assert_eq!((schema, requests), (0, 0), "no schema or schema request on the radio");
    assert!(umbilical > 0, "the umbilical still carries schema");
    assert!(data, "RF telemetry still reaches the station");
}

/// A FLIGHT_STATE write still reaches RF over the radio with the deny in place.
#[test]
fn managed_writes_still_cross_the_radio_with_schema_denied() {
    let bay = bay(Arc::new(AtomicUsize::new(0)));
    crate::keep_schema_exchange_off_the_radio(&bay.gs, bay.radio, &[bay.umbilical]).unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    bay.gs.enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE).unwrap();
    bay.rf.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY).unwrap();
    bay.gs.announce_discovery().unwrap();
    bay.rf.announce_discovery().unwrap();
    bay.pump(40);
    bay.gs
        .set_network_variable(
            Packet::new(ty, &[crate::telemetry_schema::endpoint("FLIGHT_STATE")], "GS", bay.now.load(Ordering::Relaxed), Arc::from([1u8])).unwrap(),
        )
        .unwrap();
    bay.pump(40);
    assert_eq!(bay.rf.get_cached_network_variable(ty).unwrap().map(|p| p.payload().to_vec()), Some(vec![1]));
}
