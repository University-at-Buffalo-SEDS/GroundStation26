// Router TX fairness between sides on the station's own queue loop.
use super::*;
use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

type Wire = Arc<Mutex<Vec<Vec<u8>>>>;

/// Station router with a radio side that refuses every frame (saturated
/// writer) and an umbilical side wired to a real Actuator Board peer router.
/// Returns each time (sorted) from queueing an ACTUATOR_COMMAND to the peer's
/// handler receiving it, and how many radio refusals happened meanwhile.
fn umbilical_latencies(radio_backlog: u32) -> (Vec<Duration>, u64) {
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    let refusals = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let refused = refusals.clone();
    let radio = host.add_side_packed_with_options(
        "rocket_comms",
        move |_| { refused.fetch_add(1, Ordering::Relaxed); Err(sedsnet::TelemetryError::Io("side tx busy")) },
        RouterSideOptions { reliable_enabled: true, ..Default::default() },
    );
    let to_peer: Wire = Default::default();
    let to_host: Wire = Default::default();
    let host_out = to_peer.clone();
    let umbilical = host.add_side_packed_with_options(
        "umbilical_comms",
        move |bytes| { host_out.lock().unwrap().push(bytes.to_vec()); Ok(()) },
        RouterSideOptions { reliable_enabled: true, ..Default::default() },
    );
    let received = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let receipts = received.clone();
    let peer = Router::new(RouterConfig::new([EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
        move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND") {
                receipts.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
            }
            Ok(())
        },
    )]).with_sender("AB"));
    let peer_out = to_host.clone();
    let peer_side = peer.add_side_packed_with_options(
        "wire",
        move |bytes| { peer_out.lock().unwrap().push(bytes.to_vec()); Ok(()) },
        RouterSideOptions { reliable_enabled: true, ..Default::default() },
    );
    let announce = |sender: &str, endpoint: &str| sedsnet::discovery::build_discovery_announce(
        sender, 1, &[crate::telemetry_schema::endpoint(endpoint)]).unwrap();
    host.rx_from_side(&announce("FC", "FLIGHT_CONTROLLER"), radio).unwrap();
    host.rx_from_side(&announce("AB", "ACTUATOR_BOARD"), umbilical).unwrap();
    peer.rx_from_side(&announce("GS", "GROUND_STATION"), peer_side).unwrap();
    host.process_all_queues_with_timeout(0).unwrap();
    let mut admitted = 0;
    for i in 0..radio_backlog {
        let packet = Packet::new(crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")], "GS", 1 + u64::from(i), Arc::from([(i % 200) as u8])).unwrap();
        admitted += u32::from(host.tx_queue(packet).is_ok());
    }
    assert_eq!(admitted, radio_backlog, "the whole radio backlog is held by the router");
    let stop = Arc::new(AtomicBool::new(false));
    let (worker_host, worker_stop) = (host.clone(), stop.clone());
    let (wire_in, wire_back) = (to_peer.clone(), to_host.clone());
    // The station's router worker loop (TX budget, RX budget, short sleep),
    // plus an ideal wire to the peer board.
    let worker = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Acquire) {
            let _ = process_router_queues(&worker_host);
            for bytes in std::mem::take(&mut *wire_in.lock().unwrap()) {
                let _ = peer.rx_packed_from_side(&bytes, peer_side);
            }
            let _ = peer.process_all_queues_with_timeout(0);
            for bytes in std::mem::take(&mut *wire_back.lock().unwrap()) {
                let _ = worker_host.rx_packed_from_side(&bytes, umbilical);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    std::thread::sleep(Duration::from_millis(50));
    let mut latencies = Vec::new();
    for i in 0..10u8 {
        let sent = std::time::Instant::now();
        host.tx_queue(Packet::new(crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")], "GS", 1000 + u64::from(i), Arc::from([i])).unwrap()).unwrap();
        let deadline = sent + Duration::from_secs(5);
        let arrived = loop {
            if let Some((_, at)) = received.lock().unwrap().iter().find(|(id, _)| *id == i).copied() { break Some(at); }
            if std::time::Instant::now() >= deadline { break None; }
            std::thread::sleep(Duration::from_millis(1));
        };
        let Some(at) = arrived else { latencies.push(Duration::from_secs(5)); break; };
        latencies.push(at - sent);
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let refused = refusals.load(Ordering::Relaxed);
    latencies.sort();
    eprintln!("UMBILICAL_LATENCY_MS backlog={radio_backlog} {:?} radio_refusals={refused}", latencies.iter().map(Duration::as_millis).collect::<Vec<_>>());
    (latencies, refused)
}
/// The station's own delivery tuning, process-wide, restored afterwards.
struct StationTuning { previous: sedsnet::config::RuntimeTuningConfig, _serial: std::sync::MutexGuard<'static, ()> }
impl StationTuning {
    fn install() -> Self {
        let guard = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = sedsnet::config::runtime_tuning_config();
        sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
        Self { previous, _serial: guard }
    }
}
impl Drop for StationTuning {
    fn drop(&mut self) { sedsnet::config::set_runtime_tuning_config(self.previous).unwrap(); }
}
#[test]
fn idle_radio_umbilical_tx_baseline() {
    let _tuning = StationTuning::install();
    let (latencies, _) = umbilical_latencies(0);
    assert_eq!(latencies.len(), 10, "{latencies:?}");
    assert!(latencies[5] < Duration::from_millis(200), "{latencies:?}");
}
/// A saturated radio holding 200 reliable commands (each waits out its full
/// delivery window toward a board that cannot take it) must leave the shared
/// end-to-end window room for the wired umbilical: a command to the Actuator
/// Board still arrives, typically within 200 ms. At the SDK's default window
/// of 32 it never arrived within 5 s.
#[test]
fn refused_radio_backlog_does_not_starve_umbilical_tx() {
    let _tuning = StationTuning::install();
    let (latencies, refused) = umbilical_latencies(200);
    assert!(refused > 0, "the radio really was saturated");
    // Every command arrives; the typical one in under 200 ms, and even under
    // a fully loaded parallel test run none waits a second (starved commands
    // never arrived at all).
    assert_eq!(latencies.len(), 10, "an umbilical command never arrived: {latencies:?}");
    assert!(latencies[5] < Duration::from_millis(200), "umbilical commands waited behind refused radio frames: {latencies:?}");
    assert!(latencies[9] < Duration::from_secs(1), "{latencies:?}");
}
