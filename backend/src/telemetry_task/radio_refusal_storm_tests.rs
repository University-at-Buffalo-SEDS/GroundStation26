//! A modeled RFD uplink: the station's own router and radio budget, a slow
//! paced writer and a Flight Computer router on the far end, all in one
//! virtual clock. Used to measure refusal churn, receipt/time-sync delivery
//! and large-frame progress on the busy radio.
use super::*;
use sedsnet::config::DataType;
use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
use sedsnet::timesync::{TimeSyncConfig, TimeSyncRole};
use std::sync::atomic::AtomicU64;

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

#[derive(Debug, Default)]
pub(super) struct UplinkReport {
    pub refusals: u64,
    pub refusals_by_reason: serde_json::Value,
    pub offers: u64,
    pub written_frames: u64,
    pub written_bytes: u64,
    pub timesync_announces: u64,
    pub receipts: u64,
    pub duplicate_receipts: u64,
    pub large_frames: u64,
    pub max_receipt_wait_ms: u64,
    pub flight_states_sent: u64,
    pub flight_states_delivered: u64,
    pub fc_stream_errors: u64,
    pub commands_sent: u64,
    pub commands_delivered: u64,
    pub command_latency_p50_ms: u64,
    pub command_latency_max_ms: u64,
    pub virtual_ms: u64,
    /// Age (receive time minus creation stamp) of what the FC received.
    pub heartbeats_received: u64,
    pub heartbeat_age_max_ms: u64,
    /// Longest time between consecutive heartbeats arriving at the FC.
    /// FC GND_TIMEOUT is 3 s.
    pub heartbeat_gap_max_ms: u64,
    /// Time from the last heartbeat arrival to the end of the run.
    pub heartbeat_tail_ms: u64,
    pub gps_received: u64,
    pub gps_age_max_ms: u64,
    pub park_bytes_peak: u64,
}

/// `air_bytes_per_ms`: the station's paced share of the RFD air link.
pub(super) fn run_uplink(virtual_ms: u64, air_bytes_per_ms: f64) -> UplinkReport {
    run_uplink_with(virtual_ms, air_bytes_per_ms, 0)
}

/// `station_load_per_s`: extra ordinary GS->FC frames per second (GPS_DATA),
/// to drive the uplink into the saturated state seen on the bench.
pub(super) fn run_uplink_with(virtual_ms: u64, air_bytes_per_ms: f64, station_load_per_s: u64) -> UplinkReport {
    run_uplink_model(virtual_ms, air_bytes_per_ms, station_load_per_s, false, 100)
}

/// `tell_budget`: the writer publishes its paced rate to the radio budget,
/// as the production writer does.
pub(super) fn run_uplink_model(
    virtual_ms: u64,
    air_bytes_per_ms: f64,
    station_load_per_s: u64,
    tell_budget: bool,
    fc_state_period_ms: u64,
) -> UplinkReport {
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    let clock = Arc::new(AtomicU64::new(1_000));
    let gs_clock = clock.clone();
    let delivered = Arc::new(AtomicU64::new(0));
    let delivered_in = delivered.clone();
    let gs_handler = EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("GROUND_STATION"),
        move |pkt: &Packet| {
            if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_STATE") {
                delivered_in.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        },
    );
    let gs = Arc::new(Router::new_with_clock(
        RouterConfig::new([gs_handler])
            .with_sender("GS")
            .with_preferred_discovery_master("GS")
            .with_timesync(TimeSyncConfig {
                role: TimeSyncRole::Source,
                priority: 50,
                ..TimeSyncConfig::default()
            }),
        Box::new(move || gs_clock.load(Ordering::Acquire)),
    ));
    let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
    if tell_budget {
        budget.set_paced_bytes_per_s((air_bytes_per_ms * 1000.0) as u64);
    }
    let offers = Arc::new(AtomicU64::new(0));
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let callback_budget = budget.clone();
    let callback_offers = offers.clone();
    let radio = gs.add_side_packed_with_priority_and_options(
        "rocket_comms",
        move |bytes, priority| {
            callback_offers.fetch_add(1, Ordering::Relaxed);
            callback_budget.try_send(&tx, priority, bytes)
        },
        RouterSideOptions { reliable_enabled: false, ..Default::default() }
            .with_small_packet_transport(192),
    );
    // A wired side with several boards, so the station's own discovery
    // frames toward the radio are realistically large.
    let umbilical = gs.add_side_packed_with_options(
        "umbilical_comms",
        |_| Ok(()),
        RouterSideOptions { reliable_enabled: true, ..Default::default() }
            .with_small_packet_transport(1024),
    );
    for (sender, endpoint) in [
        ("AB", "ACTUATOR_BOARD"),
        ("VB", "VALVE_BOARD"),
        ("GB", "BOARD_RECOVERY_GB"),
        ("DAQ", "BOARD_RECOVERY_DAQ"),
    ] {
        let announce = sedsnet::discovery::build_discovery_announce(
            sender,
            1,
            &[crate::telemetry_schema::endpoint(endpoint)],
        )
        .unwrap();
        gs.rx_from_side(&announce, umbilical).unwrap();
    }

    let commands_seen: Arc<Mutex<Vec<(u8, u64)>>> = Default::default();
    let commands_in = commands_seen.clone();
    let fc_now = clock.clone();
    let fc_clock = clock.clone();
    // (count, max age) for GPS_DATA and HEARTBEAT received by the FC.
    let ages: Arc<Mutex<[(u64, u64); 2]>> = Default::default();
    // (last heartbeat arrival, longest gap between arrivals)
    let gaps: Arc<Mutex<(Option<u64>, u64)>> = Default::default();
    let beat_gaps = gaps.clone();
    let gps_ages = ages.clone();
    let beat_ages = ages.clone();
    let beat_now = clock.clone();
    let fc = Router::new_with_clock(
        RouterConfig::new([
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                move |pkt: &Packet| {
                    let now = fc_now.load(Ordering::Acquire);
                    if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                        commands_in.lock().unwrap().push((pkt.payload()[0], now));
                    }
                    if pkt.data_type() == crate::telemetry_schema::data_type("GPS_DATA") {
                        let mut a = gps_ages.lock().unwrap();
                        a[0] = (a[0].0 + 1, a[0].1.max(now.saturating_sub(pkt.timestamp())));
                    }
                    Ok(())
                },
            ),
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("HEART_BEAT"),
                move |pkt: &Packet| {
                    let now = beat_now.load(Ordering::Acquire);
                    let mut a = beat_ages.lock().unwrap();
                    a[1] = (a[1].0 + 1, a[1].1.max(now.saturating_sub(pkt.timestamp())));
                    let mut g = beat_gaps.lock().unwrap();
                    if let Some(last) = g.0 {
                        g.1 = g.1.max(now.saturating_sub(last));
                    }
                    g.0 = Some(now);
                    Ok(())
                },
            ),
        ])
        .with_sender("FC")
        .with_timesync(TimeSyncConfig {
            role: TimeSyncRole::Consumer,
            priority: 100,
            ..TimeSyncConfig::default()
        }),
        Box::new(move || fc_clock.load(Ordering::Acquire)),
    );
    let downlink: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let fc_out = downlink.clone();
    let fc_side = fc.add_side_packed_with_options(
        "radio",
        move |bytes| {
            fc_out.lock().unwrap().push(bytes.to_vec());
            Ok(())
        },
        RouterSideOptions { reliable_enabled: false, ..Default::default() }
            .with_small_packet_transport(192),
    );
    gs.rx_from_side(
        &sedsnet::discovery::build_discovery_announce(
            "FC",
            1,
            &[
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                crate::telemetry_schema::endpoint("BOARD_RECOVERY_FC"),
                crate::telemetry_schema::endpoint("HEART_BEAT"),
            ],
        )
        .unwrap(),
        radio,
    )
    .unwrap();
    fc.rx_from_side(
        &sedsnet::discovery::build_discovery_announce(
            "GS",
            1,
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        )
        .unwrap(),
        fc_side,
    )
    .unwrap();
    let _ = gs.announce_discovery();

    let mut report = UplinkReport::default();
    let mut backlog = WireBacklog::new(true);
    let mut credit = 0.0f64;
    let mut oldest_receipt_wait: Option<u64> = None;
    let start = clock.load(Ordering::Acquire);
    let mut drain = 256;
    let mut seen_receipts = std::collections::HashSet::new();
    let mut commands_sent: Vec<u64> = Vec::new();
    let mut backoff = RadioRefusalBackoff::with_model_clock(clock.clone());
    for step in 0..virtual_ms {
        let now = start + step;
        clock.store(now, Ordering::Release);
        set_test_station_clock(Some(now));
        // Station load: heartbeat, and a larger status frame toward the FC.
        if step % 500 == 0 {
            let heartbeat = Packet::new(
                crate::telemetry_schema::data_type("HEARTBEAT"),
                &[crate::telemetry_schema::endpoint("HEART_BEAT")],
                "GS",
                now,
                Arc::from([]),
            )
            .unwrap();
            let _ = gs.tx_queue(heartbeat);
        }
        if station_load_per_s > 0 && step % (1000 / station_load_per_s).max(1) == 0 {
            let gps = Packet::new(
                crate::telemetry_schema::data_type("GPS_DATA"),
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
                "GS",
                now,
                Arc::from([0u8; 12]),
            )
            .unwrap();
            let _ = gs.tx_queue(gps);
        }
        // An operator command every 2 s once the link has settled.
        if step >= 5_000 && step % 2_000 == 0 {
            let id = commands_sent.len() as u8;
            let command = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
                "GS",
                now,
                Arc::from([id]),
            )
            .unwrap();
            if gs.tx_queue(command).is_ok() {
                commands_sent.push(now);
            }
        }
        // FC load: reliable ordered flight state and ordered text.
        if step % fc_state_period_ms == 0 {
            let state = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "FC",
                now,
                Arc::from([(step / fc_state_period_ms % 10) as u8]),
            )
            .unwrap();
            if fc.tx_queue(state).is_ok() {
                report.flight_states_sent += 1;
            }
        }
        let _ = gs.poll_discovery();
        let _ = gs.poll_timesync();
        process_router_queues_with_backoff(&gs, Some(&budget), &mut backoff);
        let _ = fc.poll_timesync();
        if fc.process_all_queues_with_timeout(0).is_err() {
            report.fc_stream_errors += 1;
        }
        for bytes in std::mem::take(&mut *downlink.lock().unwrap()) {
            let _ = gs.rx_packed_from_side(&bytes, radio);
        }
        // Paced writer: the radio worker's own backlog order and the
        // station's airtime share; budget released only once written.
        drain = 256;
        if !backlog.refill(&mut rx, &mut drain) {
            break;
        }
        backlog.control_burst = budget.control_burst_bytes();
        credit = (credit + air_bytes_per_ms).min(512.0);
        if !backlog.receipts.is_empty() {
            oldest_receipt_wait.get_or_insert(now);
        }
        loop {
            let selection = backlog.selection_state();
            let Some((priority, bytes)) = backlog.pop() else { break };
            let cost = (bytes.len() + 8) as f64;
            if cost > credit {
                backlog.restore_selected(selection, (priority, bytes), None);
                break;
            }
            credit -= cost;
            budget.complete(priority, &bytes);
            report.written_frames += 1;
            report.written_bytes += bytes.len() as u64;
            if bytes.len() > 128 {
                report.large_frames += 1;
            }
            if is_delivery_receipt(&bytes) {
                report.receipts += 1;
                if let Ok(pkt) = serialize::unpack_packet(&bytes) {
                    let key = (pkt.data_type(), pkt.payload().to_vec());
                    if !seen_receipts.insert(key) {
                        report.duplicate_receipts += 1;
                    }
                }
            }
            let _ = fc.rx_packed_from_side(&bytes, fc_side);
        }
        report.park_bytes_peak = report
            .park_bytes_peak
            .max(budget.diagnostics()["parked_bytes"].as_u64().unwrap_or(0));
        if backlog.receipts.is_empty() {
            if let Some(since) = oldest_receipt_wait.take() {
                report.max_receipt_wait_ms = report.max_receipt_wait_ms.max(now - since);
            }
        }
    }
    let _ = drain;
    set_test_station_clock(None);
    report.virtual_ms = virtual_ms;
    let seen = commands_seen.lock().unwrap();
    let end = start + virtual_ms;
    let mut latencies: Vec<u64> = commands_sent
        .iter()
        .enumerate()
        .map(|(id, sent)| {
            seen.iter()
                .find(|(seen_id, _)| usize::from(*seen_id) == id)
                .map_or(end - sent, |(_, at)| at - sent)
        })
        .collect();
    latencies.sort_unstable();
    report.commands_sent = commands_sent.len() as u64;
    report.commands_delivered = commands_sent.len().min(seen.len()) as u64;
    report.command_latency_p50_ms = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
    report.command_latency_max_ms = latencies.last().copied().unwrap_or(0);
    drop(seen);
    report.refusals_by_reason = budget.diagnostics()["refusals"].clone();
    report.refusals = report
        .refusals_by_reason
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum();
    report.offers = offers.load(Ordering::Relaxed);
    report.flight_states_delivered = delivered.load(Ordering::Relaxed);
    let a = *ages.lock().unwrap();
    (report.gps_received, report.gps_age_max_ms) = a[0];
    (report.heartbeats_received, report.heartbeat_age_max_ms) = a[1];
    let g = *gaps.lock().unwrap();
    report.heartbeat_gap_max_ms = if g.0.is_some() { g.1 } else { virtual_ms };
    report.heartbeat_tail_ms = g.0.map_or(virtual_ms, |last| (start + virtual_ms).saturating_sub(last));
    let fc_stats = fc.export_runtime_stats();
    for side in &fc_stats.sides {
        for ty in &side.data_types {
            if ty.data_type == DataType::TimeSyncAnnounce {
                report.timesync_announces += ty.rx_packets;
            }
        }
    }
    report
}

/// The bench state behind "commands never arrive" (Pi, a511984, RFD 25% of
/// 32 kbit/s): canonical high-priority telemetry (GPS_DATA and HEARTBEAT are
/// schema priority 255) held about 1 KB of the control allowance and, by
/// priority, went ahead of a priority-200 FLIGHT_COMMAND in the writer, so
/// the command copy never reached the air and its retries were refused as
/// duplicates. Drives the real budget and writer backlog at the station's
/// airtime share with more telemetry than the link carries.
#[test]
fn operator_command_reaches_the_air_ahead_of_saturating_telemetry() {
    crate::telemetry_schema::initialize().unwrap();
    let frame = |ty: &str, endpoint: &str, stamp: u64, payload: Vec<u8>| {
        serialize::pack_packet(
            &Packet::new(
                crate::telemetry_schema::data_type(ty),
                &[crate::telemetry_schema::endpoint(endpoint)],
                "GS",
                stamp,
                Arc::from(payload),
            )
            .unwrap(),
        )
        .to_vec()
    };
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut backlog = WireBacklog::new(true);
    let mut held: VecDeque<(u8, Vec<u8>)> = VecDeque::new(); // router-retained
    let command = frame("FLIGHT_COMMAND", "FLIGHT_CONTROLLER", 1, vec![7]);
    let mut command_written = None;
    let mut credit = 0.0f64;
    let (mut refusals, mut offers) = (0u64, 0u64);
    for now in 0..6_000u64 {
        // 40 telemetry frames per second, about 1.7x the 1 KB/s share.
        if now % 25 == 0 {
            held.push_back((253, frame("GPS_DATA", "FLIGHT_CONTROLLER", now, vec![0; 12])));
        }
        if now == 1_000 {
            held.push_front((200, command.clone()));
        }
        // The router offers what it holds, highest priority first.
        let mut order: Vec<usize> = (0..held.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(held[i].0));
        let mut sent = Vec::new();
        for i in order {
            offers += 1;
            if budget.try_send(&tx, held[i].0, &held[i].1).is_ok() {
                sent.push(i);
            } else {
                refusals += 1;
            }
        }
        sent.sort_unstable();
        for i in sent.into_iter().rev() {
            held.remove(i);
        }
        let mut drain = 256;
        backlog.refill(&mut rx, &mut drain);
        credit = (credit + 1.0).min(256.0);
        loop {
            let selection = backlog.selection_state();
            let Some((priority, bytes)) = backlog.pop() else { break };
            let cost = (bytes.len() + 16) as f64;
            if cost > credit {
                backlog.restore_selected(selection, (priority, bytes), None);
                break;
            }
            credit -= cost;
            if bytes == command && command_written.is_none() {
                command_written = Some(now);
            }
            budget.complete(priority, &bytes);
        }
    }
    let latency = command_written.map(|at| at - 1_000);
    eprintln!("COMMAND_LATENCY_MS {latency:?} offers={offers} refusals={refusals}");
    assert!(
        latency.is_some_and(|ms| ms <= 1_000),
        "the operator command must reach the air within about 1 s: {latency:?}"
    );
}

/// The full modeled uplink at the bench airtime share (1 byte per ms):
/// receipts and time sync keep flowing, every reliable flight state and
/// operator command arrives in bounded time, and refusals stay a small
/// multiple of the frames actually written (no re-offer storm).
#[test]
fn modeled_bench_uplink_keeps_receipts_time_sync_and_commands_flowing() {
    let report = run_uplink(30_000, 1.0);
    eprintln!("BENCH_UPLINK {report:#?}");
    assert_eq!(report.flight_states_delivered, report.flight_states_sent, "{report:#?}");
    assert!(report.max_receipt_wait_ms < 1_000, "{report:#?}");
    // Time sync that waited past 500 ms is dropped, not sent stale; the
    // next announce replaces it.
    assert!(report.timesync_announces >= 22, "{report:#?}");
    assert_eq!(report.commands_delivered, report.commands_sent, "{report:#?}");
    assert!(report.command_latency_max_ms < 1_000, "{report:#?}");
    assert!(
        report.refusals < 25 * report.written_frames,
        "refusals are re-offers of frames that cannot move: {report:#?}"
    );
}

/// Frames per second the RFD writer's capacity model admits for a queue of
/// small frames (receipts and heartbeats), driven like the generic worker.
fn capacity_frames_per_second(share: u64, gap_ms: u64, receipts: bool) -> (f64, f64) {
    use crate::comms::rfd_capacity::{Capacity, Defaults};
    capacity_rate(Capacity::from_defaults(115200, Defaults {
        air_bps: 32_000, overhead_bytes: 16, packet_burst_ms: 100, packet_gap_ms: gap_ms, host_share_percent: share,
    }), receipts)
}

fn capacity_rate(mut cap: crate::comms::rfd_capacity::Capacity, receipts: bool) -> (f64, f64) {
    use crate::comms::virtual_uart::WireDrain;
    let mut wire = WireDrain::default();
    let (mut frames, mut bytes) = (0u64, 0u64);
    let seconds = 20u64;
    let mut n = 0u64;
    for now in 1_000..1_000 + seconds * 1000 {
        if !wire.ready(now) { continue; }
        n += 1;
        let body = if receipts {
            serialize::pack_packet(&Packet::new(DataType::ReliableAck, &[sedsnet::config::DataEndpoint::TelemetryError], "GS", now, Arc::from(n.to_le_bytes())).unwrap()).to_vec()
        } else {
            vec![0x55; 40]
        };
        let priority = 255;
        let contract = crate::comms::radio_packet_boundary_contract(priority, &body);
        let framed = crate::comms::reference_decoder::frame_data(&body).unwrap();
        let wall = std::time::Instant::now();
        let ready = match contract {
            Some((id, true)) => cap.ready_for_receipt(wall, Some(now), id, framed.len()),
            Some((id, false)) => cap.ready_for_command(wall, Some(now), id, framed.len(), false),
            None => cap.ready_for(wall, Some(now), false),
        };
        if !ready { n -= 1; continue; }
        let len = framed.len();
        match contract {
            Some((id, true)) => { cap.committed_receipt(wall, Some(now), id, len, len, true); }
            _ => cap.committed(wall, Some(now), len),
        }
        wire.committed(now, len, 115200);
        cap.drained_virtual(wire.stop_us());
        if crate::comms::closes_radio_packet_batch(priority, &body) {
            cap.close_packet_batch_for_contract(wall, Some(now), contract);
        }
        frames += 1;
        bytes += len as u64;
    }
    (frames as f64 / seconds as f64, bytes as f64 / seconds as f64)
}

/// Bench, Normal mode, no RRLS (Pi, aed4351): after one 3 s gap in direct
/// AV-bay frames the primary requested a relay and dropped to the 15% relay
/// share, held until 5 s of continuous direct frames. Its uplink then carried
/// about 390 B/s, 6.3 ACK-sized frames per second, against about 5.8 ACKs/s
/// of demand (253 reliable AV-bay messages per minute plus retransmission
/// copies), so heartbeats, time sync and commands got no airtime and the
/// router queue grew to 14k frames. Drives the real recovery Mesh into that
/// state and the real capacity model at whatever share it then applies.
#[test]
fn primary_requesting_a_relay_still_carries_the_bench_receipt_load() {
    crate::telemetry_schema::initialize().unwrap();
    let mut primary = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 1);
    primary.tick(3_000); // no direct AV-bay frame for 3 s, no RRLS answers
    let profile_share = 25;
    let share = primary.recovery_air_share(3_000).map_or(profile_share, u64::from);
    let bench_ack_bytes_per_s = 5.8 * 67.0;
    let (frames, bytes) = capacity_frames_per_second(share, 55, true);
    assert!(
        bytes > bench_ack_bytes_per_s * 1.5,
        "share {share}%: {frames:.1} receipt frames/s, {bytes:.0} B/s against {bench_ack_bytes_per_s:.0} B/s of ACK demand"
    );
}

/// Umbilical command latency through the station's router worker loop while
/// the radio writer is full and the router keeps queueing radio frames
/// (a growing radio backlog, as on the bench at Idle in Normal mode).
fn umbilical_latency_with_saturated_radio(gated: bool) -> (Vec<u64>, u32, u64) {
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    let host = Arc::new(Router::new(
        RouterConfig::new([]).with_sender("GS").with_memory_config(crate::network_memory_config()).unwrap(),
    ));
    let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
    let (tx, _rx_held) = mpsc::unbounded_channel::<(u8, Vec<u8>)>(); // never drained
    let cb = budget.clone();
    let radio = host.add_side_packed_with_priority_and_options(
        "rocket_comms",
        move |bytes, priority| cb.try_send(&tx, priority, bytes),
        RouterSideOptions { reliable_enabled: false, ..Default::default() }.with_small_packet_transport(192),
    );
    let to_peer: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let to_host: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let out = to_peer.clone();
    let umbilical = host.add_side_packed_with_options(
        "umbilical_comms",
        move |bytes| { out.lock().unwrap().push(bytes.to_vec()); Ok(()) },
        RouterSideOptions { reliable_enabled: true, ..Default::default() },
    );
    let received = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let seen = received.clone();
    let peer = Router::new(RouterConfig::new([EndpointHandler::new_packet_handler(
        crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
        move |packet| {
            if packet.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND") {
                seen.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
            }
            Ok(())
        },
    )]).with_sender("AB"));
    let back = to_host.clone();
    let peer_side = peer.add_side_packed_with_options(
        "wire",
        move |bytes| { back.lock().unwrap().push(bytes.to_vec()); Ok(()) },
        RouterSideOptions { reliable_enabled: true, ..Default::default() },
    );
    let announce = |sender: &str, endpoint: &str| sedsnet::discovery::build_discovery_announce(
        sender, 1, &[crate::telemetry_schema::endpoint(endpoint)]).unwrap();
    host.rx_from_side(&announce("FC", "FLIGHT_CONTROLLER"), radio).unwrap();
    host.rx_from_side(&announce("AB", "ACTUATOR_BOARD"), umbilical).unwrap();
    peer.rx_from_side(&announce("GS", "GROUND_STATION"), peer_side).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (worker_host, worker_stop, worker_budget) = (host.clone(), stop.clone(), budget.clone());
    let worker = std::thread::spawn(move || {
        let mut backoff = RadioRefusalBackoff::default();
        let mut stamp = 0u64;
        while !worker_stop.load(Ordering::Acquire) {
            // A steady stream of radio-bound telemetry that cannot be written.
            stamp += 1;
            let _ = worker_host.tx_queue(Packet::new(crate::telemetry_schema::data_type("GPS_DATA"),
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")], "GS", stamp, Arc::from([0u8; 12])).unwrap());
            process_router_queues_with_backoff(&worker_host, gated.then_some(worker_budget.as_ref()), &mut backoff);
            for bytes in std::mem::take(&mut *to_peer.lock().unwrap()) {
                let _ = peer.rx_packed_from_side(&bytes, peer_side);
            }
            let _ = peer.process_all_queues_with_timeout(0);
            for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
                let _ = worker_host.rx_packed_from_side(&bytes, umbilical);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    let mut latencies = Vec::new();
    for i in 0..10u8 {
        let sent = std::time::Instant::now();
        host.tx_queue(Packet::new(crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")], "GS", 100_000 + u64::from(i), Arc::from([i])).unwrap()).unwrap();
        let arrived = loop {
            if let Some((_, at)) = received.lock().unwrap().iter().find(|(id, _)| *id == i).copied() { break Some(at); }
            if sent.elapsed() > Duration::from_secs(3) { break None; }
            std::thread::sleep(Duration::from_millis(1));
        };
        latencies.push(arrived.map_or(3_000, |at| (at - sent).as_millis() as u64));
        std::thread::sleep(Duration::from_millis(30));
    }
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    latencies.sort_unstable();
    let tx_len = host.queue_counters()[2];
    let refusals = budget.activity_counters().0;
    eprintln!("UMBILICAL_WITH_SATURATED_RADIO gated={gated} {latencies:?} tx_len={tx_len} radio_refusals={refusals} budget={}", budget.diagnostics());
    (latencies, tx_len, refusals)
}

/// A radio that cannot keep up (here: never drains) must not hold the
/// router's shared TX queue. On the bench the radio backlog reached 14k
/// router frames; the router serves its queue in priority order inside a
/// 3 ms pass, so umbilical commands and receipts waited behind refused radio
/// frames, and each pass re-offered (spun on) every one of them.
#[test]
fn saturated_radio_does_not_delay_umbilical_commands() {
    let (latencies, tx_len, refusals) = umbilical_latency_with_saturated_radio(true);
    assert!(latencies[5] < 50, "umbilical command p50 {latencies:?}");
    assert!(latencies[9] < 500, "umbilical command max {latencies:?}");
    assert!(tx_len < 64, "the router does not hold a radio backlog: tx_len {tx_len}");
    assert!(refusals < 2_000, "no refusal spin while the radio is full: {refusals}");
}

/// The FC is the only AV-bay consumer of the station HEARTBEAT. After
/// GND_TIMEOUT = 3000 ms without one it sets Lost_GroundStation, which is
/// never cleared until reset: it blocks the Startup/Armed monitor preflight
/// and makes the failure evaluator treat the mission as active (recovery.c).
/// Every 500 ms heartbeat must therefore reach the radio, keeping a loss
/// margin of 5 consecutive heartbeats; stale ones may be shed only behind a
/// fresher one.
#[test]
fn radio_keeps_the_full_heartbeat_cadence() {
    crate::telemetry_schema::initialize().unwrap();
    let clock = Arc::new(AtomicU64::new(1_000));
    let router_clock = clock.clone();
    let router = Router::new_with_clock(
        RouterConfig::new([]).with_sender("GS"),
        Box::new(move || router_clock.load(Ordering::Acquire)),
    );
    let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let cb = budget.clone();
    let side = router.add_side_packed_with_priority_and_options(
        "rocket_comms",
        move |bytes, priority| cb.try_send(&tx, priority, bytes),
        RouterSideOptions { reliable_enabled: false, ..Default::default() }.with_small_packet_transport(192),
    );
    router
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce("FC", 1, &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")]).unwrap(),
            side,
        )
        .unwrap();
    let classifier = RecoveryFrameClassifier::default();
    let mut heartbeats = Vec::new();
    for step in 0..10_000u64 {
        let now = 1_000 + step;
        clock.store(now, Ordering::Release);
        set_test_station_clock(Some(now));
        if step % 500 == 0 {
            let packet = Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
                &[crate::telemetry_schema::endpoint("HEART_BEAT"), crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
                "GS", now, Arc::from([])).unwrap();
            let _ = router.tx_queue(packet);
        }
        let _ = router.process_all_queues_with_timeout(0);
        while let Ok((priority, bytes)) = rx.try_recv() {
            classifier.classify(&bytes);
            if classifier.peek_type(&bytes) == Some(crate::telemetry_schema::data_type("HEARTBEAT")) {
                heartbeats.push(now);
            }
            budget.complete(priority, &bytes);
        }
    }
    set_test_station_clock(None);
    let gaps: Vec<u64> = heartbeats.windows(2).map(|w| w[1] - w[0]).collect();
    eprintln!("RADIO_HEARTBEATS {} gaps {gaps:?}", heartbeats.len());
    assert!(heartbeats.len() >= 19, "every 500 ms heartbeat: {heartbeats:?}");
    assert!(gaps.iter().all(|gap| *gap <= 600), "loss margin of 5 inside the FC's 3 s timeout: {gaps:?}");
}

/// A peer's DISCOVERY_SCHEMA request makes the station send its whole
/// schema: about 20 chunks of 192 bytes, four seconds of the radio share.
/// Chunks never change the compact-frame dictionary, so other frames may
/// pass them; a station heartbeat (FC timeout 3 s), time sync or telemetry
/// queued behind a schema transfer must not wait for all of it.
#[test]
fn schema_transfer_does_not_hold_frames_queued_behind_it() {
    crate::telemetry_schema::initialize().unwrap();
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    for index in 0u16..20 {
        let mut chunk = vec![0u8; 192];
        chunk[..4].copy_from_slice(b"SDT\x03");
        chunk[4..8].copy_from_slice(&77u32.to_le_bytes());
        chunk[8..10].copy_from_slice(&index.to_le_bytes());
        chunk[10..12].copy_from_slice(&20u16.to_le_bytes());
        budget.try_send(&tx, 254, &chunk).unwrap();
    }
    let heartbeat = serialize::pack_packet(&Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
        &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")], "GS", 1, Arc::from([])).unwrap()).to_vec();
    budget.try_send(&tx, 253, &heartbeat).unwrap();
    // Drain at the 25% share of 32 kbit/s: 1 byte per ms.
    let mut credit = 0.0f64;
    let mut backlog = WireBacklog::new(true);
    let mut heartbeat_at = None;
    let mut chunks_written = Vec::new();
    for now in 0..6_000u64 {
        let mut drain = 256;
        backlog.refill(&mut rx, &mut drain);
        credit = (credit + 1.0).min(256.0);
        loop {
            let selection = backlog.selection_state();
            let Some((priority, bytes)) = backlog.pop() else { break };
            let cost = (bytes.len() + 16) as f64;
            if cost > credit {
                backlog.restore_selected(selection, (priority, bytes), None);
                break;
            }
            credit -= cost;
            if bytes == heartbeat && heartbeat_at.is_none() {
                heartbeat_at = Some(now);
            }
            if bytes.starts_with(b"SDT\x03") {
                chunks_written.push(u16::from_le_bytes([bytes[8], bytes[9]]));
            }
            budget.complete(priority, &bytes);
        }
    }
    eprintln!("HEARTBEAT_BEHIND_SCHEMA_MS {heartbeat_at:?} chunks {chunks_written:?}");
    assert!(heartbeat_at.is_some_and(|ms| ms < 1_000), "heartbeat waited {heartbeat_at:?} ms");
    assert_eq!(chunks_written, (0..20).collect::<Vec<_>>(), "the schema still completes, in order");
}

/// MANAGED_VARIABLE_REQUEST was excluded from the park, and nothing may pass
/// a parked frame, so on a busy radio every request was refused on every
/// router pass and never sent (faithful simulation: 0 sent, about 230
/// refusals/s, the largest remaining spin). Within its rate share it now
/// queues behind the parked frames like any other ordinary frame.
#[test]
fn managed_variable_request_is_not_starved_by_parked_frames() {
    use sedsnet::config::{DataEndpoint, DataType};
    let request = serialize::pack_packet(&Packet::new(DataType::ManagedVariableRequest, &[DataEndpoint::Discovery], "GS", 1, Arc::from(vec![1u8; 4])).unwrap()).to_vec();
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    budget.try_send(&tx, 0, &[1; 200]).unwrap();
    budget.try_send(&tx, 0, &[2; 100]).unwrap();
    assert_eq!(budget.diagnostics()["parked_frames"], 1);
    budget.try_send(&tx, 254, &request).expect("a request within its share is accepted");
    let mut written = Vec::new();
    while let Ok((p, f)) = rx.try_recv() {
        budget.complete(p, &f);
        written.push(f);
        while let Ok((p, f)) = rx.try_recv() {
            budget.complete(p, &f);
            written.push(f);
        }
    }
    assert!(written.iter().any(|f| *f == request), "the request reaches the writer");
}

/// The RFD modem already arbitrates the air (ASYNC, LBT, its own copies).
/// Host pacing only has to keep the 4 KiB modem buffer from overflowing
/// (no RTS/CTS), so with the default profile one host alone may use the
/// modem's whole serial-to-air drain rate (usable_air_bps, measured on the
/// bench with S5=0). Before, the GS held itself to a 25% share
/// with 100 ms bursts and 55 ms gaps (about 620 B/s of receipts) and the RF
/// to 35%, each reserving airtime the other was not using.
#[test]
fn one_host_alone_can_use_the_modem_drain_rate() {
    crate::telemetry_schema::initialize().unwrap();
    let profile = crate::comms::rfd_profile::Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
    // S5=0 bench pair (2026-11-10, scripts/rfd_link_test.py): one-way 2.9 KB/s
    // lost 0.5% and 5.8 KB/s lost 6%, well under air_bps/8; the profile's
    // usable_air_bps is that measured drain.
    let drain = (profile.modem.air_bps / (1 + profile.modem.broadcast_retries))
        .min(profile.transport.usable_air_bps) / 8;
    let cap = crate::comms::rfd_capacity::Capacity::from_defaults(
        115200,
        crate::comms::rfd_capacity::Defaults {
            air_bps: profile.transport.usable_air_bps,
            overhead_bytes: profile.transport.frame_overhead_bytes,
            packet_burst_ms: profile.transport.packet_burst_ms,
            packet_gap_ms: profile.transport.packet_gap_ms,
            host_share_percent: profile.transport.host_default_share_percent,
        },
    );
    let (frames, bytes) = capacity_rate(cap, true);
    // Air bytes include the per-frame modem overhead the pacing charges.
    let air = bytes + frames * profile.transport.frame_overhead_bytes as f64;
    eprintln!("ONE_HOST_RATE frames/s {frames:.1} B/s {bytes:.0} air B/s {air:.0} drain {drain}");
    assert!(air >= drain as f64 * 0.7, "one host uses at least 70% of the {drain} B/s drain: {air:.0} B/s");
    assert!(air <= drain as f64, "and never more than the modem can drain: {air:.0} B/s");
}

/// A retry of a frame whose identical bytes are still waiting for the writer
/// (a reliable retransmission, or a command the router offers again) was
/// refused on every router pass until the first copy was written: on the
/// bench Pi 24k duplicate_command refusals. The copy already waiting carries
/// the same bytes first, so the retry is accepted and dropped.
#[test]
fn identical_retry_of_a_waiting_frame_is_coalesced_not_refused() {
    crate::telemetry_schema::initialize().unwrap();
    let command = serialize::pack_packet(&Packet::new(crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
        &[crate::telemetry_schema::endpoint("AV_BAY_UNDERGLOW_OWNER")], "GS", 1, Arc::from([1_u8])).unwrap()).to_vec();
    let telemetry = vec![5u8; 60];
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    budget.try_send(&tx, 16, &command).unwrap();
    // Fill the ordinary allowance so the next frame parks.
    budget.try_send(&tx, 0, &[1; 250]).unwrap();
    budget.try_send(&tx, 0, &telemetry).unwrap();
    assert_eq!(budget.diagnostics()["parked_frames"], 1);
    for _ in 0..100 {
        budget.try_send(&tx, 16, &command).expect("a command retry is not refused");
        budget.try_send(&tx, 0, &telemetry).expect("a retry of a parked frame is not refused");
    }
    let d = budget.diagnostics();
    assert_eq!(d["refusals"]["duplicate_command"], 0);
    assert_eq!(d["parked_frames"], 1, "the parked copy is not duplicated");
    let mut written = Vec::new();
    while let Ok((p, f)) = rx.try_recv() {
        budget.complete(p, &f);
        written.push(f);
    }
    assert_eq!(written.iter().filter(|f| **f == command).count(), 1);
    assert_eq!(written.iter().filter(|f| **f == telemetry).count(), 1);
    // Once written, the same bytes are a new transmission again.
    budget.try_send(&tx, 16, &command).unwrap();
    assert_eq!(rx.try_recv().unwrap().1, command);
}

/// The large-frame reservation made every smaller ordinary frame leave room
/// for a refused large one. With the radio park every frame the writer cannot
/// take yet waits in order, so the reservation only ever refused offers:
/// 157k large_frame_reservation refusals on the bench Pi. The radio has no
/// reservation; nothing is refused for one.
#[test]
fn radio_never_refuses_for_a_large_frame_reservation() {
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    budget.try_send(&tx, 0, &[1; 250]).unwrap();
    // Fill the park with frames that cannot be shed (schema chunks).
    let mut transfer = 0u32;
    loop {
        let mut chunk = vec![0u8; 1000];
        chunk[..4].copy_from_slice(b"SDT\x03");
        chunk[4..8].copy_from_slice(&transfer.to_le_bytes());
        chunk[8..10].copy_from_slice(&1u16.to_le_bytes());
        chunk[10..12].copy_from_slice(&2u16.to_le_bytes());
        transfer += 1;
        if budget.try_send(&tx, 254, &chunk).is_err() {
            break;
        }
    }
    // Top the park up with short frames until it takes no more.
    for k in 0u32.. {
        let mut filler = vec![9u8; 8];
        filler[..4].copy_from_slice(&k.to_le_bytes());
        if budget.try_send(&tx, 0, &filler).is_err() {
            break;
        }
    }
    // A starving frame and then short frames meet the full park.
    let _ = budget.try_send(&tx, 0, &[2; 200]);
    for n in 0..50u8 {
        let _ = budget.try_send(&tx, 0, &[3, n, 0, 0, 0, 0, 0, 0]);
    }
    assert_eq!(budget.diagnostics()["refusals"]["large_frame_reservation"], 0);
    assert!(budget.large_waiter.lock().unwrap().is_none(), "no reservation is held");
    drop(rx);
}

/// Both modems share one half-duplex air channel. Each host paces only its own
/// UART, just under one modem's drain rate. When both hosts are busy at once
/// (a schema transfer from the GS while the RF streams telemetry), each modem
/// only gets part of the air; without RTS/CTS its 4 KiB buffer must not
/// overflow, and receipts, time sync and commands must still reach the air
/// within 1 s. Each host hears the other's packets (broadcast), so the air
/// they use is visible to it.
struct SharedAirHost {
    cap: crate::comms::rfd_capacity::Capacity,
    heard: crate::comms::rfd_capacity::HeardAir,
    wire: crate::comms::virtual_uart::WireDrain,
    /// Bytes in the modem buffer, oldest first: (remaining, created, priority).
    modem: std::collections::VecDeque<(usize, u64, bool)>,
    modem_bytes: usize,
    peak: usize,
    overflowed: usize,
    priority_waiting: std::collections::VecDeque<u64>,
    priority_worst_ms: u64,
    bulk_delivered: usize,
    bulk: bool,
}

fn shared_air_run(a_bulk: bool, b_bulk: bool) -> [SharedAirHost; 2] {
    const MODEM_BUFFER: usize = 4096;
    const AIR_PACKET: usize = 256;
    let profile = crate::comms::rfd_profile::Profile::parse(include_bytes!("../../comms/rfd-profile.json")).unwrap();
    let copies = 1 + profile.modem.broadcast_retries;
    let overhead = profile.transport.frame_overhead_bytes as usize;
    let air_bits_per_ms = profile.modem.air_bps / 1000;
    let new_host = |bulk| SharedAirHost {
        cap: crate::comms::rfd_capacity::Capacity::from_defaults(115200, crate::comms::rfd_capacity::Defaults {
            air_bps: profile.transport.usable_air_bps,
            overhead_bytes: profile.transport.frame_overhead_bytes,
            packet_burst_ms: profile.transport.packet_burst_ms,
            packet_gap_ms: profile.transport.packet_gap_ms,
            host_share_percent: profile.transport.host_default_share_percent,
        }),
        heard: Default::default(),
        wire: Default::default(),
        modem: Default::default(),
        modem_bytes: 0,
        peak: 0,
        overflowed: 0,
        priority_waiting: Default::default(),
        priority_worst_ms: 0,
        bulk_delivered: 0,
        bulk,
    };
    let mut hosts = [new_host(a_bulk), new_host(b_bulk)];
    let wall = std::time::Instant::now();
    let mut air_free_at = 0u64;
    let mut turn = 0usize;
    for now in 0..20_000u64 {
        // The air: one packet at a time, modems alternate when both hold data.
        if now >= air_free_at {
            let order = [turn, 1 - turn];
            if let Some(&tx) = order.iter().find(|&&i| hosts[i].modem_bytes > 0) {
                turn = 1 - tx;
                let mut take = AIR_PACKET.min(hosts[tx].modem_bytes);
                let sent = take;
                while take > 0 {
                    let front = hosts[tx].modem.front_mut().unwrap();
                    let part = take.min(front.0);
                    front.0 -= part;
                    take -= part;
                    if front.0 == 0 {
                        let (_, created, priority) = hosts[tx].modem.pop_front().unwrap();
                        if priority {
                            let host = &mut hosts[tx];
                            host.priority_worst_ms = host.priority_worst_ms.max(now - created);
                        }
                    }
                }
                hosts[tx].modem_bytes -= sent;
                hosts[tx].bulk_delivered += sent;
                let air_ms = (copies as usize * (sent + overhead) * 8).div_ceil(air_bits_per_ms as usize) as u64;
                air_free_at = now + air_ms.max(1);
                // The other host's modem hears the first copy.
                let other = &mut hosts[1 - tx];
                other.heard.charge(&mut other.cap, wall, Some(now), (sent + overhead) as u64);
            }
        }
        for host in hosts.iter_mut() {
            // A receipt, time sync or command every 100 ms.
            if now % 100 == 0 {
                host.priority_waiting.push_back(now);
            }
            if !host.wire.ready(now) {
                continue;
            }
            let (len, created, priority) = if let Some(&created) = host.priority_waiting.front() {
                (60usize, created, true)
            } else if host.bulk {
                (200, now, false)
            } else {
                continue;
            };
            if !host.cap.ready_for(wall, Some(now), priority) {
                continue;
            }
            if priority {
                host.priority_waiting.pop_front();
            }
            host.cap.committed(wall, Some(now), len);
            host.wire.committed(now, len, 115200);
            host.cap.drained_virtual(host.wire.stop_us());
            if host.modem_bytes + len > MODEM_BUFFER {
                host.overflowed += len;
                continue;
            }
            host.modem.push_back((len, created, priority));
            host.modem_bytes += len;
            host.peak = host.peak.max(host.modem_bytes);
        }
    }
    for host in hosts.iter_mut() {
        if let Some(&created) = host.priority_waiting.front() {
            host.priority_worst_ms = host.priority_worst_ms.max(20_000 - created);
        }
    }
    hosts
}

#[test]
fn both_hosts_saturated_never_overflow_a_modem_and_keep_priority_frames_under_1s() {
    for (a_bulk, b_bulk) in [(true, true), (false, true), (true, false)] {
        let hosts = shared_air_run(a_bulk, b_bulk);
        for (index, host) in hosts.iter().enumerate() {
            eprintln!(
                "SHARED_AIR bulk=({a_bulk},{b_bulk}) host {index}: peak {} B overflowed {} B priority worst {} ms delivered {} B/s",
                host.peak, host.overflowed, host.priority_worst_ms, host.bulk_delivered / 20
            );
            assert_eq!(host.overflowed, 0, "host {index} overflowed its modem (bulk {a_bulk},{b_bulk})");
            assert!(host.priority_worst_ms < 1_000, "host {index} priority frames waited {} ms", host.priority_worst_ms);
            if host.bulk {
                assert!(host.bulk_delivered / 20 >= 800, "a busy host keeps a fair part of the air: {} B/s", host.bulk_delivered / 20);
            }
        }
    }
}


/// Bench Pi (GS dev, both modems GLOBAL_RETRIES 10): the station's air share
/// is a few hundred bytes per second, less than its own demand. The park
/// filled to its 1024-frame cap (52 KB, about 50 s of airtime), so heartbeats,
/// time sync and AV-bay requests were sent tens of seconds late, and every
/// router pass re-offered refused frames (130k HEARTBEAT re-offers in 20 s).
/// Old dev in this model: park 14.5 KB, heartbeats 20 s and telemetry 29 s
/// old on arrival. With the writer's paced rate known, what waits is bounded
/// by airtime: only the newest replaceable frame of a stream waits, stale
/// ones are dropped, and control bursts leave ordinary frames a turn. This
/// link is oversubscribed (receipts alone take half of it), so a heartbeat
/// can still wait a few seconds behind discovery and schema frames.
#[test]
fn slow_bench_air_keeps_queued_frames_within_two_seconds() {
    // The model is deterministic alone; run it in its own process so state
    // other in-process tests leave behind cannot change its refusal count.
    if crate::operator_queue::isolated_native_case(module_path!(), "slow_bench_air_keeps_queued_frames_within_two_seconds") {
        return;
    }
    // FC: four reliable flight states per second (bench AV bay: about 4.2
    // reliable messages per second), so receipts alone take half the air.
    let report = run_uplink_model(40_000, 0.4, 20, true, 250);
    eprintln!("SLOW_BENCH_UPLINK {report:#?}");
    // About 1.5 s of air (600 B at 400 B/s), plus at most one template
    // install per best-effort template (GPS, heartbeat), which never waits
    // for the cap.
    assert!(report.park_bytes_peak <= 720, "the park holds about 1.5 s of air: {report:#?}");
    assert!(report.heartbeats_received >= 3, "{report:#?}");
    assert!(report.heartbeat_age_max_ms <= 5_000, "heartbeats arrive within the FC's reach: {report:#?}");
    // The link is oversubscribed: receipts, heartbeats (FC GND_TIMEOUT),
    // time sync and commands win, so the station's own GPS-to-FC telemetry
    // may not get through at all; what does arrive is never stale.
    assert!(report.gps_received == 0 || report.gps_age_max_ms <= 2_500, "telemetry is never sent stale: {report:#?}");
    assert!(report.heartbeat_gap_max_ms <= 2_000, "heartbeats keep the FC alive: {report:#?}");
    assert!(report.timesync_announces >= 20, "time sync keeps flowing: {report:#?}");
    assert_eq!(report.flight_states_delivered, report.flight_states_sent, "{report:#?}");
    assert_eq!(report.commands_delivered, report.commands_sent, "{report:#?}");
    assert!(report.command_latency_max_ms < 1_000, "{report:#?}");
    // Re-offers of deferred bulk frames (schema chunks, discovery) stay an
    // order of magnitude below the bench storm (about 4000/s). With heartbeat
    // and time sync now always served first on this oversubscribed link, more
    // bulk SDT waits and is re-offered (about 300/s).
    assert!(report.refusals * 1000 / report.virtual_ms <= 400, "{report:#?}");
}

/// Faithful bench run (GS fix, bench RF, S5=10): with the downlink dead, the
/// station's own ordered retransmits (197 BOARD_RECOVERY_REQUEST in 75 s)
/// filled the control lane ahead of receipts: 47 RELIABLE_ACK sent against
/// 26.5k refused offers. Receipts and time sync responses are small and are
/// what stop the far side retransmitting, so they go before the station's
/// own retransmittable requests; ABORT and operator commands stay first.
#[test]
fn receipts_and_time_sync_go_ahead_of_own_ordered_retransmits() {
    crate::telemetry_schema::initialize().unwrap();
    let pack = |ty: sedsnet::config::DataType, endpoint: sedsnet::config::DataEndpoint, stamp: u64, payload: Vec<u8>| {
        serialize::pack_packet(&Packet::new(ty, &[endpoint], "GS", stamp, Arc::from(payload)).unwrap()).to_vec()
    };
    let request = |n: u64| pack(crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        crate::telemetry_schema::endpoint("BOARD_RECOVERY_RF"), n, vec![n as u8; 32]);
    let ack = |n: u64| pack(DataType::ReliableAck, sedsnet::config::DataEndpoint::TelemetryError, n, n.to_le_bytes().to_vec());
    let time_sync = pack(DataType::TimeSyncResponse, sedsnet::config::DataEndpoint::TimeSync, 9, vec![0; 32]);
    let abort = pack(crate::telemetry_schema::data_type("ABORT"), crate::telemetry_schema::endpoint("ABORT"), 7,
        b"SEDS_MANUAL_ABORT_V1:K".to_vec());
    let mut backlog = WireBacklog::new(true);
    for n in 0..6 {
        backlog.push(255, request(n), false);
    }
    backlog.push(255, time_sync.clone(), false);
    for n in 0..3 {
        backlog.push(255, ack(n), false);
    }
    backlog.push(255, abort.clone(), false);
    let mut order = Vec::new();
    while let Some((_, bytes)) = backlog.pop() {
        order.push(bytes);
    }
    let types: Vec<_> = order
        .iter()
        .map(|b| serialize::peek_frame_info_unchecked(b).unwrap().envelope.ty)
        .collect();
    let first_request = types
        .iter()
        .position(|t| *t == crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"))
        .unwrap();
    let last_ack = types.iter().rposition(|t| *t == DataType::ReliableAck).unwrap();
    let time_sync_at = types.iter().position(|t| *t == DataType::TimeSyncResponse).unwrap();
    assert_eq!(types[0], crate::telemetry_schema::data_type("ABORT"), "ABORT first: {types:?}");
    assert!(last_ack < first_request, "every receipt before the first own retransmit: {types:?}");
    assert!(time_sync_at < first_request, "time sync before own retransmits: {types:?}");
    assert_eq!(order.len(), 11);
}

/// Takeover fullnet (turn-taking): a BOARD_RECOVERY_REQUEST queued ahead of
/// an operator ABORT was held for the station's air turn and, at the head of
/// the critical lane, kept the ABORT (never held for a turn) queued 260 ms.
#[test]
fn abort_leaves_the_critical_lane_ahead_of_an_earlier_control() {
    crate::telemetry_schema::initialize().unwrap();
    let pack = |ty: sedsnet::config::DataType, endpoint: sedsnet::config::DataEndpoint, stamp: u64, payload: Vec<u8>| {
        serialize::pack_packet(&Packet::new(ty, &[endpoint], "GS", stamp, Arc::from(payload)).unwrap()).to_vec()
    };
    let request = pack(crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        crate::telemetry_schema::endpoint("BOARD_RECOVERY_RF"), 1, vec![1; 32]);
    let abort = pack(crate::telemetry_schema::data_type("ABORT"), crate::telemetry_schema::endpoint("ABORT"), 2,
        b"SEDS_MANUAL_ABORT_V1:K".to_vec());
    let mut backlog = WireBacklog::new(true);
    backlog.push(255, request.clone(), false);
    backlog.push(255, abort.clone(), false);
    assert_eq!(backlog.pop_critical().map(|(_, b)| b), Some(abort), "ABORT first");
}

/// Faithful (GS dev 93b8401, newest firmware, S5=1): RF/FC/PB re-anchored
/// to TIME_SYNC_ANNOUNCE frames that reached the air seconds to tens of
/// seconds after creation (park about 50 KB), so after about 100 s the AV
/// bay stamped packets 90-130 s in the past. A time sync frame that has
/// waited more than 0.5 s since the radio accepted it is dropped at the
/// writer, never sent late; the next announce replaces it.
#[test]
fn time_sync_waiting_more_than_half_a_second_is_dropped_not_sent() {
    crate::telemetry_schema::initialize().unwrap();
    let announce = serialize::pack_packet(&Packet::new(DataType::TimeSyncAnnounce,
        &[sedsnet::config::DataEndpoint::TimeSync], "GS", 5, Arc::from(vec![0u8; 16])).unwrap()).to_vec();
    let budget = TxQueueBudget::new_radio(2048, 256);
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    set_test_station_clock(Some(10_000));
    budget.try_send(&tx, 255, &announce).unwrap();
    let (priority, bytes) = rx.try_recv().unwrap();
    // Still fresh at 0.4 s: written.
    set_test_station_clock(Some(10_400));
    assert!(!discard_retired_write(Some(&budget), None, priority, &bytes));
    // Waited 0.6 s for the writer: dropped and released.
    set_test_station_clock(Some(10_600));
    assert!(discard_retired_write(Some(&budget), None, priority, &bytes), "a stale time sync is never sent");
    assert_eq!(budget.pending.load(Ordering::Acquire), 0);
    set_test_station_clock(None);
}

/// Faithful HITL Reset all: parked SDT chunks of a schema transfer were
/// counted as recovery control, so recovery quiesce reported "a control frame
/// is still being written on the AV-bay radio link" while only schema bytes
/// waited. A chunk transfer is control only when the frame it carries is (or
/// when its first chunk was never seen).
#[test]
fn schema_chunks_never_hold_recovery_quiesce_but_chunked_controls_do() {
    crate::telemetry_schema::initialize().unwrap();
    let chunk = |id: u32, index: u16, total: u16, data: &[u8]| {
        let mut bytes = b"SDT\x03".to_vec();
        bytes.extend(id.to_le_bytes());
        bytes.extend(index.to_le_bytes());
        bytes.extend(total.to_le_bytes());
        bytes.extend_from_slice(data);
        bytes.extend(crc32fast::hash(&bytes).to_le_bytes());
        bytes
    };
    let schema = serialize::pack_packet(&Packet::new(DataType::DiscoverySchema,
        &[sedsnet::config::DataEndpoint::Discovery], "GS", 1, Arc::from(vec![7u8; 600])).unwrap()).to_vec();
    let request = serialize::pack_packet(&Packet::new(crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST"),
        &[crate::telemetry_schema::endpoint("BOARD_RECOVERY_RF")], "GS", 1, Arc::from(vec![1u8; 32])).unwrap()).to_vec();
    let classifier = RecoveryFrameClassifier::default();
    assert!(!classifier.classify(&chunk(5, 0, 2, &schema[..400])), "schema chunk 0");
    assert!(!classifier.classify(&chunk(5, 1, 2, &schema[400..])), "schema chunk 1");
    assert!(classifier.classify(&chunk(7, 0, 2, &request[..request.len() / 2])), "chunked control");
    assert!(classifier.classify(&chunk(7, 1, 2, &request[request.len() / 2..])), "rest of chunked control");
    assert!(classifier.classify(&chunk(9, 1, 2, &[0; 40])), "a transfer whose first chunk was not seen");
    // The ledger recovery quiesce reads: only schema chunks owned -> quiet.
    let activity = TransportTxActivity::default();
    for (index, part) in [(0u16, &schema[..400]), (1, &schema[400..])] {
        let bytes = chunk(11, index, 2, part);
        activity.try_enqueue_owned(classifier.classify(&bytes), bytes, |_| Ok(())).unwrap();
    }
    assert!(!activity.has_pending_control(), "parked schema chunks do not block Reset all");
}

/// HITL Reset all (S5=1, 23ac94c): quiesce still reported "a control frame is
/// still being written on the AV-bay radio link" in one cycle. Time sync is an
/// SDK-internal type outside the station schema, so it counted as recovery
/// control, and the station sends it about once a second. Time sync is never
/// work owed to the boards a recovery resets.
#[test]
fn time_sync_frames_are_not_recovery_control() {
    crate::telemetry_schema::initialize().unwrap();
    for ty in [DataType::TimeSyncAnnounce, DataType::TimeSyncRequest, DataType::TimeSyncResponse] {
        let frame = serialize::pack_packet(&Packet::new(ty, &[sedsnet::config::DataEndpoint::TimeSync], "GS", 1,
            Arc::from(vec![0u8; if ty == DataType::TimeSyncResponse { 32 } else { 16 }])).unwrap()).to_vec();
        assert!(!is_recovery_control_frame(&frame), "{ty:?}");
    }
    let abort = serialize::pack_packet(&Packet::new(crate::telemetry_schema::data_type("ABORT"),
        &[crate::telemetry_schema::endpoint("ABORT")], "GS", 1, Arc::from(&b"SEDS_MANUAL_ABORT_V1:K"[..])).unwrap()).to_vec();
    assert!(is_recovery_control_frame(&abort));
}

/// Found chasing a nondeterministic refusal count: depending on packet
/// nonces the SDK re-sends GPS and HEARTBEAT template installs while schema
/// chunks fill the time-bounded park. Installs were never replaceable-dropped
/// and did not fit, so they were refused on every router pass for about 9 s
/// (thousands of re-offers) and no heartbeat reached the FC for that long
/// (its ground timeout is 3 s). A best-effort template install now always
/// parks: only the newest install of each template waits, so this is bounded
/// by the number of templates.
#[test]
fn heartbeat_template_install_parks_even_when_schema_chunks_fill_the_park() {
    crate::telemetry_schema::initialize().unwrap();
    let install = |stamp: u64| {
        let frame = serialize::pack_packet(&Packet::new(crate::telemetry_schema::data_type("HEARTBEAT"),
            &[crate::telemetry_schema::endpoint("HEART_BEAT")], "GS", stamp, Arc::from([])).unwrap()).to_vec();
        let mut bytes = b"SDT\x01".to_vec();
        bytes.push(9);
        bytes.extend_from_slice(&frame);
        bytes.extend(crc32fast::hash(&bytes).to_le_bytes());
        bytes
    };
    let budget = TxQueueBudget::new_radio(2048, 256);
    budget.set_paced_bytes_per_s(400);
    let (tx, _rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    budget.try_send(&tx, 0, &[1; 250]).unwrap();
    // Schema chunks (never shed) fill the 1.5 s park.
    let mut transfer = 0u32;
    loop {
        let mut chunk = vec![0u8; 192];
        chunk[..4].copy_from_slice(b"SDT\x03");
        chunk[4..8].copy_from_slice(&transfer.to_le_bytes());
        chunk[8..10].copy_from_slice(&1u16.to_le_bytes());
        chunk[10..12].copy_from_slice(&2u16.to_le_bytes());
        transfer += 1;
        if budget.try_send(&tx, 254, &chunk).is_err() {
            break;
        }
    }
    let parked = budget.diagnostics()["parked_frames"].as_u64().unwrap();
    budget.try_send(&tx, 255, &install(1)).expect("a heartbeat install is never refused for a full park");
    assert_eq!(budget.diagnostics()["parked_frames"].as_u64().unwrap(), parked + 1);
    budget.try_send(&tx, 255, &install(2)).expect("a newer install of the same template replaces it");
    assert_eq!(budget.diagnostics()["parked_frames"].as_u64().unwrap(), parked + 1, "one install per template");
}

/// FC GND_TIMEOUT is 3 s and Lost_GroundStation latches, so heartbeats must
/// keep arriving with gaps well under 3 s even while schema chunks saturate
/// the radio and other threads make packets concurrently (which changes the
/// SDK's process-wide packet nonce sequence; one sequence made the SDK re-send
/// template installs and once held heartbeats off the air for about 9 s).
#[test]
fn fc_heartbeat_gap_stays_under_two_seconds_under_schema_saturation_and_nonce_churn() {
    if crate::operator_queue::isolated_native_case(module_path!(), "fc_heartbeat_gap_stays_under_two_seconds_under_schema_saturation_and_nonce_churn") {
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    let mut worst = Vec::new();
    for _ in 0..8 {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn_stop = stop.clone();
        let churn = std::thread::spawn(move || {
            let mut n = 0u64;
            while !churn_stop.load(Ordering::Relaxed) {
                n += 1;
                let _ = Packet::new(DataType::ReliableAck, &[sedsnet::config::DataEndpoint::TelemetryError], "AB", n, Arc::from([0u8; 8]));
            }
        });
        let report = run_uplink_model(40_000, 0.4, 20, true, 250);
        stop.store(true, Ordering::Relaxed);
        let _ = churn.join();
        worst.push((report.heartbeat_gap_max_ms, report.heartbeats_received, report.heartbeat_tail_ms));
    }
    eprintln!("FC_HEARTBEAT_GAPS {worst:?}");
    assert!(
        worst.iter().all(|(gap, _, tail)| *gap <= 2_000 && *tail <= 2_000),
        "worst FC heartbeat gaps (gap ms, count, ms since the last at the end): {worst:?}"
    );
}

/// How the RF owner treats the station's frames in the FLIGHT_STATE bay.
#[derive(Clone, Copy, PartialEq)]
enum RfOwner {
    Answers,
    Silent,
    /// Frames reach RF only after this many ms (a slow radio).
    Late(u64),
}

/// The station with RF on the radio side and GB on the umbilical, both
/// FLIGHT_STATE readers, on real SDK routers with the station's delivery
/// tuning (retry span 250 ms x 18). Returns, for each write in
/// `writes` (ms after the first write, value), the ms after that write at
/// which GB first held that value (None: never within `run_ms`), and every
/// value GB applied in order.
fn flight_state_bay(rf_mode: RfOwner, writes: &[(u64, u8)], run_ms: u64) -> (Vec<Option<u64>>, Vec<u8>) {
    use sedsnet::router::NetworkVariablePermissions;
    let _tuning = StationTuning::install();
    crate::telemetry_schema::initialize().unwrap();
    let ty = crate::telemetry_schema::data_type("FLIGHT_STATE");
    let host = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    host.enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE).unwrap();
    crate::telemetry_task::refresh_host_network_time(&host, get_current_timestamp_ms());
    let applied = Arc::new(Mutex::new(Vec::<(u8, std::time::Instant)>::new()));
    let mut peers = Vec::new();
    let mut outbound = Vec::new();
    for name in ["RF", "GB"] {
        let peer = Arc::new(Router::new(RouterConfig::new([]).with_sender(name)));
        peer.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY).unwrap();
        if name == "GB" {
            let seen = applied.clone();
            peer.on_network_variable_update(ty, move |packet| {
                seen.lock().unwrap().push((packet.payload()[0], std::time::Instant::now()));
                Ok(())
            })
            .unwrap();
        }
        let to_peer = Arc::new(Mutex::new(Vec::<(std::time::Instant, Vec<u8>)>::new()));
        let out = to_peer.clone();
        host.add_side_packed(name, move |bytes| {
            out.lock().unwrap().push((std::time::Instant::now(), bytes.to_vec()));
            Ok(())
        });
        let to_host = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let back = to_host.clone();
        peer.add_side_packed("GS", move |bytes| {
            back.lock().unwrap().push(bytes.to_vec());
            Ok(())
        });
        peers.push(peer);
        outbound.push((to_peer, to_host));
    }
    let mut backoff = RadioRefusalBackoff::default();
    let mut pump = |rf: RfOwner| {
        process_router_queues_with_backoff(&host, None, &mut backoff);
        for (side, (peer, (to_peer, to_host))) in peers.iter().zip(&outbound).enumerate() {
            let ready: Vec<Vec<u8>> = {
                let mut queue = to_peer.lock().unwrap();
                let due = |at: &std::time::Instant| match (side, rf) {
                    (0, RfOwner::Silent) => None,
                    (0, RfOwner::Late(ms)) => Some(at.elapsed() >= Duration::from_millis(ms)),
                    _ => Some(true),
                };
                let mut ready = Vec::new();
                queue.retain(|(at, bytes)| match due(at) {
                    None => false,
                    Some(true) => {
                        ready.push(bytes.clone());
                        false
                    }
                    Some(false) => true,
                });
                ready
            };
            for bytes in ready {
                let _ = peer.rx_packed_from_side(&bytes, 0);
            }
            let _ = peer.process_all_queues_with_timeout(0);
            for bytes in std::mem::take(&mut *to_host.lock().unwrap()) {
                let _ = host.rx_packed_from_side(&bytes, side);
            }
        }
    };
    for peer in &peers {
        peer.announce_discovery().unwrap();
    }
    host.announce_discovery().unwrap();
    for _ in 0..50 {
        pump(RfOwner::Answers);
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = std::time::Instant::now();
    let mut sent_at = Vec::new();
    let mut next = 0;
    while start.elapsed() < Duration::from_millis(run_ms) {
        if let Some(&(at, value)) = writes.get(next)
            && start.elapsed() >= Duration::from_millis(at)
        {
            let stamp = host.network_time_ms().unwrap();
            host.set_network_variable(
                Packet::new(ty, &[crate::telemetry_schema::endpoint("FLIGHT_STATE")], "GS", stamp, Arc::from([value]))
                    .unwrap()
                    .with_nonce(0x4000 + next as u16),
            )
            .unwrap();
            sent_at.push(std::time::Instant::now());
            next += 1;
        }
        pump(rf_mode);
        std::thread::sleep(Duration::from_millis(2));
    }
    let applied = applied.lock().unwrap().clone();
    let delivered = writes
        .iter()
        .zip(&sent_at)
        .map(|(&(_, value), sent)| {
            applied
                .iter()
                .find(|(seen, at)| *seen == value && at >= sent)
                .map(|(_, at)| (*at - *sent).as_millis() as u64)
        })
        .collect();
    (delivered, applied.iter().map(|(value, _)| *value).collect())
}

fn reliable_retry_span_ms() -> u64 {
    let tuning = crate::network_delivery_tuning();
    u64::from(tuning.reliable_retransmit_ms) * u64::from(tuning.reliable_max_retries + 1)
}

/// accept4: the station wrote Idle, RF never acknowledged it, and the Abort
/// written 3 s later waited behind it. When Idle ran out of retries on RF,
/// the SDK dropped the Abort unsent; GB and DAQ (which take an Abort only
/// from FLIGHT_STATE) stayed in Idle for ~100 s. The Abort must still reach
/// GB, within the retry span of the write it waited behind.
#[test]
fn abort_behind_an_unacknowledged_flight_state_still_reaches_gb() {
    let span = reliable_retry_span_ms();
    let (delivered, applied) = flight_state_bay(RfOwner::Silent, &[(0, 1), (500, 15)], span + 3_000);
    eprintln!("ABORT_BEHIND_SILENT_RF span={span} delivered={delivered:?} applied={applied:?}");
    assert!(delivered[0].is_some_and(|ms| ms < 500), "Idle reaches GB at once: {delivered:?}");
    let abort = delivered[1].unwrap_or_else(|| panic!("the Abort never reached GB: applied {applied:?}"));
    assert!(abort <= span + 750, "Abort reached GB {abort} ms after it was written (span {span} ms)");
    assert_eq!(applied.last(), Some(&15), "GB ends in Aborted: {applied:?}");
}

/// No exhaustion: RF acknowledges, but late (two thirds of the retry span).
/// The Abort waits behind Idle for as long as RF takes (the SDK's ordering),
/// never longer than the retry span, and is delivered once.
#[test]
fn abort_behind_a_slow_rf_owner_reaches_gb_within_the_retry_span() {
    let span = reliable_retry_span_ms();
    let (delivered, applied) = flight_state_bay(RfOwner::Late(span * 2 / 3), &[(0, 1), (500, 15)], span + 3_000);
    eprintln!("ABORT_BEHIND_SLOW_RF span={span} delivered={delivered:?} applied={applied:?}");
    let abort = delivered[1].unwrap_or_else(|| panic!("the Abort never reached GB: applied {applied:?}"));
    assert!(abort <= span + 750, "Abort reached GB {abort} ms after it was written (span {span} ms)");
    assert_eq!(applied, vec![1, 15], "Idle then Aborted, once each");
}

/// A later write wins: Abort, then Idle, both stranded behind a silent RF.
/// Only the latest (Idle) may be sent again, never the stale Abort after it.
#[test]
fn resent_flight_state_is_the_latest_value_not_a_stale_one() {
    let span = reliable_retry_span_ms();
    let (delivered, applied) = flight_state_bay(RfOwner::Silent, &[(0, 1), (300, 15), (600, 0)], 2 * span + 3_000);
    eprintln!("LATEST_WINS span={span} delivered={delivered:?} applied={applied:?}");
    assert_eq!(applied.last(), Some(&0), "the latest write (Startup) wins: {applied:?}");
    assert!(!applied.contains(&15), "a superseded Abort is never replayed after a newer write: {applied:?}");
}

/// Nothing stuck: every write reaches GB once, in order, without resends.
#[test]
fn flight_state_writes_with_answering_owners_are_neither_repeated_nor_reordered() {
    let (delivered, applied) =
        flight_state_bay(RfOwner::Answers, &[(0, 1), (300, 2), (600, 15), (900, 0), (1_200, 1)], 2_500);
    eprintln!("ANSWERING_OWNERS delivered={delivered:?} applied={applied:?}");
    assert!(delivered.iter().all(|ms| ms.is_some_and(|ms| ms < 300)), "{delivered:?}");
    assert_eq!(applied, vec![1, 2, 15, 0, 1], "each write once, in order");
}
