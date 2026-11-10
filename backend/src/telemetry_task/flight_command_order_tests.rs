//! FLIGHT_COMMAND is an Ordered reliable stream: a command waits until the FC
//! has acknowledged the one before it, and when that earlier command runs out
//! of retries the SDK retires the stream and drops what is queued behind it.
//! These cases show what that does to an operator Deploy. Real SDK routers,
//! station delivery tuning; no hardware.
use super::*;
use super::radio_io::{process_router_queues_with_backoff, RadioRefusalBackoff};
use sedsnet::router::{EndpointHandler, RouterConfig};

struct StationTuning {
    previous: sedsnet::config::RuntimeTuningConfig,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl StationTuning {
    fn install() -> Self {
        let guard = crate::RUNTIME_TUNING_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = sedsnet::config::runtime_tuning_config();
        sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
        Self { previous, _serial: guard }
    }
}
impl Drop for StationTuning {
    fn drop(&mut self) {
        sedsnet::config::set_runtime_tuning_config(self.previous).unwrap();
    }
}

fn reliable_retry_span_ms() -> u64 {
    let tuning = crate::network_delivery_tuning();
    u64::from(tuning.reliable_retransmit_ms) * u64::from(tuning.reliable_max_retries + 1)
}

/// The station and the FC on real SDK routers with the station's delivery
/// tuning (250 ms x 18). The FC is unreachable for the first `outage_ms`
/// (frames to it are lost, as in a radio outage before the RRLS relay takes
/// over). FLIGHT_COMMAND is an Ordered reliable stream, so a command waits
/// until the one before it is acknowledged. Returns, per command written at
/// (ms after start, FC byte), the ms after its write at which the FC handler
/// first received it (None: never within `run_ms`).
fn flight_command_bay(outage_ms: u64, writes: &[(u64, u8)], run_ms: u64) -> Vec<Option<u64>> {
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_COMMAND");
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    let got = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let seen = got.clone();
    let fc = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            move |packet| {
                seen.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
                Ok(())
            },
        )])
        .with_sender("FC"),
    ));
    let to_fc = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let out = to_fc.clone();
    host.add_side_packed("FC", move |bytes| {
        out.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let to_host = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let back = to_host.clone();
    fc.add_side_packed("GS", move |bytes| {
        back.lock().unwrap().push(bytes.to_vec());
        Ok(())
    });
    let mut backoff = RadioRefusalBackoff::default();
    let start = std::time::Instant::now();
    let mut pump = |link_up: bool| {
        process_router_queues_with_backoff(&host, None, &mut backoff);
        for bytes in std::mem::take(&mut *to_fc.lock().unwrap()) {
            if link_up {
                let _ = fc.rx_packed_from_side(&bytes, 0);
            }
        }
        let _ = fc.process_all_queues_with_timeout(0);
        for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
            if link_up {
                let _ = host.rx_packed_from_side(&bytes, 0);
            }
        }
    };
    fc.announce_discovery().unwrap();
    host.announce_discovery().unwrap();
    for _ in 0..50 {
        pump(true);
        std::thread::sleep(Duration::from_millis(1));
    }
    got.lock().unwrap().clear();
    let start_run = std::time::Instant::now();
    let _ = start;
    let mut sent_at = Vec::new();
    let mut next = 0;
    while start_run.elapsed() < Duration::from_millis(run_ms) {
        if let Some(&(at, byte)) = writes.get(next)
            && start_run.elapsed() >= Duration::from_millis(at)
        {
            host.tx_queue(crate::telemetry_task::controller_packet(&host, ty, &[byte]).unwrap()).unwrap();
            sent_at.push(std::time::Instant::now());
            next += 1;
        }
        pump(start_run.elapsed() >= Duration::from_millis(outage_ms));
        std::thread::sleep(Duration::from_millis(2));
    }
    let got = got.lock().unwrap().clone();
    writes
        .iter()
        .zip(&sent_at)
        .map(|(&(_, byte), sent)| got.iter().find(|(b, at)| *b == byte && at >= sent).map(|(_, at)| (*at - *sent).as_millis() as u64))
        .collect()
}

/// A Deploy pressed while an earlier FC command is still unacknowledged waits
/// behind it (FLIGHT_COMMAND is Ordered). On a link that returns within the
/// earlier command's retry span the Deploy is delivered as soon as the link
/// returns; it never waits longer than that span.
#[test]
fn deploy_behind_an_unacknowledged_fc_command_waits_at_most_one_retry_span() {
    let span = reliable_retry_span_ms();
    let delivered = flight_command_bay(span * 2 / 3, &[(0, 14), (500, 12)], span + 3_000);
    eprintln!("DEPLOY_BEHIND_SLOW_LINK span={span} delivered={delivered:?}");
    let deploy = delivered[1].unwrap_or_else(|| panic!("Deploy never reached the FC: {delivered:?}"));
    assert!(deploy <= span, "Deploy reached the FC {deploy} ms after it was pressed (span {span} ms)");
}

/// The strand: the earlier FC command runs out of retries during an outage
/// that ends inside the Deploy's own retry span. The SDK retires the Ordered
/// stream and drops the Deploy queued behind it unsent, so the FC never gets
/// the Deploy although the link came back while the operator's press was still
/// live. The Deploy must reach the FC.
#[test]
#[ignore = "reproduces the Ordered FLIGHT_COMMAND strand (the Deploy is dropped unsent); enabled with its fix"]
fn deploy_behind_an_exhausted_fc_command_still_reaches_the_fc() {
    let span = reliable_retry_span_ms();
    let delivered = flight_command_bay(span + 1_000, &[(0, 14), (2_000, 12)], span + 6_000);
    eprintln!("DEPLOY_BEHIND_EXHAUSTED span={span} delivered={delivered:?}");
    let deploy = delivered[1].unwrap_or_else(|| panic!("Deploy was dropped behind the exhausted command: {delivered:?}"));
    assert!(deploy <= span + 750, "Deploy reached the FC {deploy} ms after it was pressed (span {span} ms)");
}
