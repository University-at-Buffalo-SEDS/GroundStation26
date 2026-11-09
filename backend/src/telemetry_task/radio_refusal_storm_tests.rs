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
        static SERIAL: Mutex<()> = Mutex::new(());
        let guard = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
}

/// `air_bytes_per_ms`: the station's paced share of the RFD air link.
pub(super) fn run_uplink(virtual_ms: u64, air_bytes_per_ms: f64) -> UplinkReport {
    run_uplink_with(virtual_ms, air_bytes_per_ms, 0)
}

/// `station_load_per_s`: extra ordinary GS->FC frames per second (GPS_DATA),
/// to drive the uplink into the saturated state seen on the bench.
pub(super) fn run_uplink_with(virtual_ms: u64, air_bytes_per_ms: f64, station_load_per_s: u64) -> UplinkReport {
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
    let fc = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
            move |pkt: &Packet| {
                if pkt.data_type() == crate::telemetry_schema::data_type("FLIGHT_COMMAND") {
                    commands_in.lock().unwrap().push((pkt.payload()[0], fc_now.load(Ordering::Acquire)));
                }
                Ok(())
            },
        )])
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
        if step % 100 == 0 {
            let state = Packet::new(
                crate::telemetry_schema::data_type("FLIGHT_STATE"),
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                "FC",
                now,
                Arc::from([(step / 100 % 10) as u8]),
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
        if backlog.receipts.is_empty() {
            if let Some(since) = oldest_receipt_wait.take() {
                report.max_receipt_wait_ms = report.max_receipt_wait_ms.max(now - since);
            }
        }
    }
    let _ = drain;
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
    assert!(report.timesync_announces >= 25, "{report:#?}");
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
    use crate::comms::virtual_uart::WireDrain;
    let mut cap = Capacity::from_defaults(115200, Defaults {
        air_bps: 32_000, overhead_bytes: 16, packet_burst_ms: 100, packet_gap_ms: gap_ms, host_share_percent: share,
    });
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
