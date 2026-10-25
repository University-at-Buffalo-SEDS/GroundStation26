//! Uses real Mesh state machines and SEDSnet packed routers. The RF application
//! boundary below unwraps the documented RR envelope; MCU execution, modem MAC
//! collision/backoff/FEC and antenna propagation still require linked firmware.
use super::{
    shared_air::{Config, SharedAir},
    *,
};
use sedsnet::{
    MessageClass, MessageDataType, MessageElement, ReliableMode, config,
    packet::Packet,
    router::{EndpointHandler, Router, RouterConfig},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
};

#[path = "../comms/virtual_uart.rs"]
mod virtual_uart;
type Drivers = Arc<Mutex<[virtual_uart::WireDrain; 3]>>;
static FIFO_REJECT_LOGGED: AtomicU64 = AtomicU64::new(0);

fn driver_send(
    air: &Arc<Mutex<SharedAir>>,
    drivers: &Drivers,
    node: usize,
    now: u64,
    bytes: Vec<u8>,
) -> sedsnet::TelemetryResult<()> {
    let mut driver = drivers.lock().unwrap();
    if !driver[node].ready(now) {
        return Err(sedsnet::TelemetryError::Io("side tx busy"));
    }
    let length = bytes.len();
    // RTS/CTS is disabled in the captured profile. The modem can drop a frame
    // after the UART accepted it; the SDK must recover using its actual ACKs.
    if let Err(bytes) = air.lock().unwrap().submit(node, bytes) {
        if FIFO_REJECT_LOGGED.fetch_add(1, Ordering::Relaxed) < 16 {
            let kind = sedsnet::wire_format::peek_frame_info(&bytes[4..])
                .ok()
                .map(|frame| frame.envelope.ty.as_u32());
            eprintln!(
                "RFD_FIFO_REFUSAL node={node} virtual_ms={now} bytes={} type={kind:?}",
                bytes.len()
            );
        }
    }
    driver[node].committed(now, length, 115_200);
    Ok(())
}
fn process(router: &Router) {
    if let Err(error) = router.process_all_queues_with_timeout(0) {
        assert!(
            matches!(error, sedsnet::TelemetryError::Io("side tx busy")),
            "unexpected router failure: {error:?}"
        );
    }
}

fn serial(frame: &Outgoing) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.bytes.len() + 4);
    out.extend_from_slice(if frame.command {
        &[0xa6, 0x5b]
    } else {
        &[0xa5, 0x5a]
    });
    out.extend_from_slice(&(frame.bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(&frame.bytes);
    out
}
fn flush_mesh(
    mesh: &Shared,
    air: &Arc<Mutex<SharedAir>>,
    drivers: &Drivers,
    node: usize,
    now: u64,
) {
    for _ in 0..4 {
        if !drivers.lock().unwrap()[node].ready(now) {
            break;
        }
        let Some(transmission) = mesh.lock().unwrap().begin_transmission(now) else {
            break;
        };
        let accepted = driver_send(air, drivers, node, now, serial(transmission.frame())).is_ok();
        mesh.lock()
            .unwrap()
            .finish_transmission(transmission, accepted);
        if !accepted {
            break;
        }
    }
}
fn run_case(mode: ReliableMode, fifo: usize, overhead_bits: u32, duration: u64) {
    FIFO_REJECT_LOGGED.store(0, Ordering::Relaxed);
    crate::telemetry_schema::initialize().unwrap();
    let station = crate::telemetry_schema::endpoint("GROUND_STATION");
    let owner = config::register_endpoint("RFD_AIR_PROBE_OWNER", false).unwrap();
    let probe = config::register_data_type_with_description(
        &format!("RFD_AIR_LATENCY_PROBE_{mode:?}"),
        "diagnostic only",
        MessageElement::Static(1, MessageDataType::UInt32, MessageClass::Data),
        &[owner],
        mode,
        16,
    )
    .unwrap();
    let gps = crate::telemetry_schema::data_type("GPS_DATA");
    let air = Arc::new(Mutex::new(
        SharedAir::new(
            &[1, 2, 3],
            Config {
                air_bits_per_second: 64_000,
                uart_baud: 115_200,
                air_overhead_bits: overhead_bits,
                turn_us: 2500,
                tx_fifo_bytes: fifo,
                rx_fifo_bytes: fifo,
                max_burst_bytes: 4096,
            },
        )
        .unwrap(),
    ));
    let drivers: Drivers = Arc::new(Mutex::new(std::array::from_fn(|_| Default::default())));
    let primary = Arc::new(Mutex::new(Mesh::new(Role::Primary, 1)));
    let recovery = Arc::new(Mutex::new(Mesh::new(Role::Rrls, 2)));
    let clock = Arc::new(AtomicU64::new(1000));
    let effects = Arc::new(Mutex::new(BTreeMap::<u32, Vec<u64>>::new()));
    let telemetry = Arc::new(Mutex::new(Vec::<u64>::new()));
    let received = telemetry.clone();
    let time = clock.clone();
    let gs = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(station, move |p| {
            if p.data_type() == gps {
                received
                    .lock()
                    .unwrap()
                    .push(time.load(Ordering::SeqCst) - 1000);
            }
            Ok(())
        })])
        .with_sender("GS"),
        {
            let time = clock.clone();
            Box::new(move || time.load(Ordering::SeqCst))
        },
    );
    let observed = effects.clone();
    let time = clock.clone();
    let rf = Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(owner, move |p| {
            if p.data_type() == probe {
                let seq = u32::from_le_bytes(p.payload().try_into().unwrap());
                observed
                    .lock()
                    .unwrap()
                    .entry(seq)
                    .or_default()
                    .push(time.load(Ordering::SeqCst) - 1000);
            }
            Ok(())
        })])
        .with_sender("RF"),
        {
            let time = clock.clone();
            Box::new(move || time.load(Ordering::SeqCst))
        },
    );
    let medium = air.clone();
    let mesh = primary.clone();
    let time = clock.clone();
    let uart = drivers.clone();
    let gs_side = gs.add_side_packed_small_packets(
        "rfd",
        move |bytes| {
            let now = time.load(Ordering::SeqCst) - 1000;
            let frame = mesh.lock().unwrap().local_tx(false, bytes, now).unwrap();
            driver_send(&medium, &uart, 0, now, serial(&frame))
        },
        crate::RADIO_SIDE_MAX_FRAME_BYTES,
    );
    let medium = air.clone();
    let time = clock.clone();
    let uart = drivers.clone();
    let rf_side = rf.add_side_packed_small_packets(
        "rfd",
        move |bytes| {
            driver_send(
                &medium,
                &uart,
                1,
                time.load(Ordering::SeqCst) - 1000,
                serial(&Outgoing {
                    command: false,
                    bytes: bytes.to_vec(),
                }),
            )
        },
        crate::RADIO_SIDE_MAX_FRAME_BYTES,
    );
    gs.announce_discovery().unwrap();
    rf.announce_discovery().unwrap();
    let mut issued = BTreeMap::<u32, (u64, u64)>::new();
    let mut seen_pending = BTreeSet::<u64>::new();
    let mut retired = BTreeMap::new();
    let mut stream_decoders: [crate::comms::reference_decoder::Decoder; 3] =
        std::array::from_fn(|_| Default::default());
    let mut seq = 0u32;
    for now in (0..=duration).step_by(10) {
        clock.store(now + 1000, Ordering::SeqCst);
        if now % 10_000 == 0 {
            eprintln!(
                "RFD_REFERENCE_PROGRESS fifo={fifo} overhead={overhead_bits} virtual_ms={now}"
            );
        }
        let direct = !(10_000..20_000).contains(&now) && !(45_000..60_000).contains(&now);
        let rrls_connected = !(80_000..90_000).contains(&now);
        {
            let mut a = air.lock().unwrap();
            a.advance_to(now * 1000).unwrap();
            a.set_link(0, 1, direct);
            a.set_link(1, 0, direct);
            for node in 0..2 {
                a.set_link(2, node, rrls_connected);
                a.set_link(node, 2, rrls_connected);
            }
        }
        for node in 0..3 {
            let mut ready = Vec::new();
            while let Some(bytes) = air.lock().unwrap().receive(node) {
                ready.extend(bytes);
            }
            // Modem reads may split or coalesce application frames. Exercise
            // the production decoder with varying byte boundaries; the air
            // model still does not claim vendor raw-stream packetization.
            let width = 1 + (now as usize / 10 + node) % 31;
            let mut frames = Vec::new();
            for part in ready.chunks(width) {
                frames.extend(stream_decoders[node].push(part, now));
            }
            frames.extend(stream_decoders[node].push(&[], now));
            for (command, data) in frames {
                let data = data.as_slice();
                if node == 0 {
                    let delivered = {
                        let mut mesh = primary.lock().unwrap();
                        mesh.incoming(command, data, now)
                    };
                    if let Some(frame) = delivered {
                        if !frame.command {
                            gs.rx_packed_from_side(&frame.bytes, gs_side).unwrap();
                        }
                    }
                } else if node == 2 {
                    recovery.lock().unwrap().incoming(command, data, now);
                } else if command {
                    // The actual RF firmware unwraps these before SEDSnet RX.
                    if let Some((kind, data)) = decode(data) {
                        if kind == AV_BAY_DATA {
                            rf.rx_packed_from_side(data, rf_side).unwrap();
                        }
                    }
                }
            }
        }
        primary.lock().unwrap().tick(now);
        recovery.lock().unwrap().tick(now);
        flush_mesh(&primary, &air, &drivers, 0, now);
        flush_mesh(&recovery, &air, &drivers, 2, now);
        if now % 100 == 0 && now < duration - 5000 {
            rf.log_queue_ts(gps, now + 1000, &[43.0f32, -78.0, now as f32])
                .unwrap();
        }
        if now >= 6000 && now < duration - 10_000 && now % 1000 == 0 {
            seq += 1;
            let packet = Packet::new(
                probe,
                &[owner],
                "GS",
                now + 1000,
                seq.to_le_bytes().to_vec().into(),
            )
            .unwrap();
            let id = packet.packet_id();
            gs.tx_queue(packet).unwrap();
            issued.insert(seq, (now, id));
        }
        process(&gs);
        process(&rf);
        let pending: BTreeSet<_> = gs
            .export_reliable_pending_stats()
            .into_iter()
            .filter(|p| p.data_type == probe)
            .map(|p| p.packet_id)
            .collect();
        seen_pending.extend(pending.iter().copied());
        for (&seq, &(_, id)) in &issued {
            if seen_pending.contains(&id) && !pending.contains(&id) {
                retired.entry(seq).or_insert(now);
            }
        }
    }
    let effects = effects.lock().unwrap();
    let mut worst = 0;
    let mut worst_ack = 0;
    for (&seq, &(created, _)) in &issued {
        let times = effects.get(&seq).unwrap_or_else(|| {
            panic!(
                "lost diagnostic command{seq} created{created}, fifo{fifo} overhead{overhead_bits}"
            )
        });
        assert_eq!(times.len(), 1, "duplicate command{seq}");
        let ack = *retired
            .get(&seq)
            .unwrap_or_else(|| panic!("ACK did not retire command{seq}"));
        // Explicit handover windows include liveness timeout plus outstanding
        // ordered predecessors. Stable periods still retain the500ms gate.
        let stable = created < 9500 || (26_000..44_500).contains(&created) || created >= 66_000;
        if stable {
            worst = worst.max(times[0] - created);
            worst_ack = worst_ack.max(ack - created);
        }
    }
    assert!(
        worst <= 500,
        "stable delivery latency{worst}ms, fifo{fifo} overhead{overhead_bits}"
    );
    assert!(
        worst_ack <= 500,
        "stable ACK latency{worst_ack}ms, fifo{fifo} overhead{overhead_bits}"
    );
    assert!(
        gs.export_reliable_pending_stats()
            .iter()
            .all(|p| p.data_type != probe)
    );
    let reports = telemetry.lock().unwrap();
    for range in [
        5000..9500,
        16_000..19_000,
        26_000..44_000,
        51_000..59_000,
        66_000..79_000,
        82_000..88_000,
        95_000..duration - 5000,
    ] {
        let selected: Vec<_> = reports
            .iter()
            .copied()
            .filter(|n| range.contains(n))
            .collect();
        assert!(selected.len() > 10, "missing telemetry during{range:?}");
        assert!(
            selected[0] - range.start <= 500,
            "late telemetry at start of{range:?}"
        );
        assert!(
            range.end - selected[selected.len() - 1] <= 500,
            "stale telemetry at end of{range:?}"
        );
        assert!(
            selected.windows(2).all(|v| v[1] - v[0] <= 500),
            "stale telemetry during{range:?}"
        );
    }
    assert!(
        stream_decoders.iter().all(|d| d.rejected == 0),
        "unexpected production UART decoder rejection"
    );
    let parser_peaks: Vec<_> = stream_decoders.iter().map(|d| d.peak_bytes).collect();
    let air = air.lock().unwrap();
    let counters: Vec<_> = (0..3).map(|n| air.counters(n)).collect();
    assert!(counters.iter().all(|c| c.0 <= fifo && c.1 <= fifo));
    if fifo >= 16_384 {
        assert!(
            counters.iter().all(|c| c.2 == 0 && c.3 == 0),
            "unexpected finite FIFO losses in no-overflow reference:{counters:?}"
        );
    }

    assert!(
        air.completed_air_bits <= duration * 64,
        "medium exceeded64kbps aggregate"
    );
    println!(
        "RFD_REFERENCE mode={mode:?} fifo={fifo} overhead_bits={overhead_bits} virtual_ms={duration} commands={} worst_stable_delivery_ms={worst} worst_stable_ACK_ms={worst_ack} telemetry={} counters={counters:?} parser_peak_bytes={parser_peaks:?}",
        issued.len(),
        reports.len()
    );
}

#[test]
fn finite_three_radio_mesh_recovers_actual_reliable_commands_and_receipts() {
    // The standalone SDK defaults differ from the deployed station's tuning.
    // Run this global-config fixture in an isolated test process so unrelated
    // tests cannot change those atomics during a virtual-time campaign.
    if std::env::var_os("GS_RFD_REFERENCE_CHILD").is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rrls::air_campaign::finite_three_radio_mesh_recovers_actual_reliable_commands_and_receipts", "--nocapture", "--test-threads=1"])
            .env("GS_RFD_REFERENCE_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let started = std::time::Instant::now();
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if started.elapsed() > std::time::Duration::from_secs(30) {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("isolated shared-air campaign timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("{stdout}");
        eprintln!("{stderr}");
        assert!(
            output.status.success() && stdout.contains("RFD_REFERENCE_DONE profiles=6"),
            "shared-air campaign failed: {stdout}\n{stderr}"
        );
        return;
    }
    sedsnet::config::set_runtime_tuning_config(crate::network_delivery_tuning()).unwrap();
    for mode in [ReliableMode::Ordered, ReliableMode::Unordered] {
        for (fifo, overhead, duration) in [
            (4096, 0, 120_000),
            (4096, 160, 120_000),
            (16_384, 512, 600_000),
        ] {
            run_case(mode, fifo, overhead, duration);
        }
    }
    println!("RFD_REFERENCE_DONE profiles=6");
}
