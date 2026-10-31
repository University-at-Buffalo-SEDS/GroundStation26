//! Actual generic-worker cold metadata bootstrap; no production timing claim.
use super::*;
use crate::comms::{CommsDevice, UartComms};
use sedsnet::router::{RetryTrackerLimits, RouterConfig, RouterSideOptions};
use serialport::SerialPort;
use std::io::Read;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

fn set_clock(path: &std::path::Path, clock: &AtomicU64, now: u64) {
    let next = path.with_extension("next");
    std::fs::write(&next, now.to_string()).unwrap();
    std::fs::rename(next, path).unwrap();
    clock.store(now, Ordering::SeqCst);
}
fn address(n: u32) -> Vec<u8> {
    use sedsnet::discovery::{AddressAdvertisement, LinkCapabilities};
    let ad = AddressAdvertisement {
        hostname: format!("COLD{n}"),
        address: n + 100,
        requested_address: n + 100,
        mode: 0,
        state: 1,
        birth_ms: 0,
        owner_hash: u64::from(n + 1),
        reachable_endpoints: vec![],
        reachable_network_variables: vec![],
        reachable_timesync_sources: vec![],
        link_capabilities: LinkCapabilities {
            version: 1,
            flags: 0,
            profile: 0,
            max_frame_bytes: 192,
            compact_header_target_bytes: 40,
            max_side_transport_templates: 16,
        },
    };
    let packet = sedsnet::discovery::build_discovery_address("GS", 0, &ad).unwrap();
    sedsnet::discovery::decode_discovery_address(&packet).unwrap();
    serialize::pack_packet(&packet).to_vec()
}
#[tokio::test]
async fn cold_routing_cannot_hide_paid_bootstrap_before_catalog_owner_starts() {
    const NAME: &str = "telemetry_task::radio_io::budget_bootstrap_tests::cold_routing_cannot_hide_paid_bootstrap_before_catalog_owner_starts";
    if std::env::var_os("GS_BUDGET_COLD_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!(
            "gs-budget-cold-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("clock"), "0").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("GS_BUDGET_COLD_CHILD", "1")
            .env("GS_SIM_CLOCK_FILE", dir.join("clock"))
            .env("GS_SIMULATED_SERIAL_PTY", "1")
            .env("GS_RRLS_ROLE", "primary")
            .env_remove("GS_RADIO_AIR_SHARE_PERCENT")
            .env_remove("GS_RADIO_AIR_BIT_RATE_BPS")
            .env("GS_SIM_TRACE_RELIABLE", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= until {
                child.kill().unwrap();
                panic!("bounded cold worker child timeout");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let result = child.wait_with_output().unwrap();
        std::fs::write(dir.join("stdout"), &result.stdout).unwrap();
        std::fs::write(dir.join("stderr"), &result.stderr).unwrap();
        println!("COLD_BOOTSTRAP_ARTIFACTS {}", dir.display());
        print!("{}", String::from_utf8_lossy(&result.stdout));
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
        assert!(result.status.success());
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    crate::comms::rfd_profile::initialize(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "cold-private-pty".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    })
    .unwrap();
    let path = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    let clock = Arc::new(AtomicU64::new(0));
    let sdk_clock = clock.clone();
    let router = Arc::new(Router::new_with_clock(
        RouterConfig::new([]).with_sender("GS"),
        Box::new(move || sdk_clock.load(Ordering::SeqCst)),
    ));
    let (sender, receiver) = mpsc::unbounded_channel();
    let budget = Arc::new(TxQueueBudget::new(32768));
    let mut routing_bytes = 0;
    for n in 0..8 {
        let bytes = address(n);
        routing_bytes += bytes.len();
        budget.try_send(&sender, 255, &bytes).unwrap();
    }
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_copy = accepted.clone();
    let q = budget.clone();
    let send = sender.clone();
    let (side, _) = router
        .add_side_packed_leased_with_priority_and_options(
            "cold-radio",
            move |lease, priority| {
                if lease.bytes().starts_with(b"SDT\x03") {
                    q.try_send_leased(&send, priority, lease)?;
                    accepted_copy.fetch_add(1, Ordering::SeqCst);
                }
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: false,
                ..Default::default()
            }
            .with_small_packet_transport(192),
            RetryTrackerLimits::default(),
        )
        .unwrap();
    router.clear_tx_queue();
    let catalog = include_bytes!("../test-fixtures/root-default-catalog.bin");
    assert_eq!(
        serialize::unpack_packet(catalog).unwrap().data_type(),
        sedsnet::config::DataType::DiscoverySchema
    );
    router
        .tx_packed_queue(Arc::from(catalog.as_slice()))
        .unwrap();
    router.dispatch_tx_queue_with_timeout(0).unwrap();
    assert!(accepted.load(Ordering::SeqCst) > 1);
    let strict_clock = clock.clone();
    let strict = Router::new_with_clock(
        RouterConfig::new([])
            .with_sender("STRICT")
            .with_reliable_enabled(false),
        Box::new(move || strict_clock.load(Ordering::SeqCst)),
    );
    let decoded = Arc::new(Mutex::new(Vec::<Packet>::new()));
    let decoded_out = decoded.clone();
    strict.add_side_packet("decoded-canonical", move |packet| {
        if packet.data_type() == sedsnet::config::DataType::DiscoverySchema {
            decoded_out.lock().unwrap().push(packet.clone());
        }
        Ok(())
    });
    let strict_side = strict.add_side_packed_with_options(
        "actual-radio",
        |_| Ok(()),
        RouterSideOptions {
            reliable_enabled: false,
            ..Default::default()
        }
        .with_small_packet_transport(192),
    );
    let original = serialize::unpack_packet(catalog).unwrap();
    let direct_ad = address(99);
    let mut primary = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 42);
    primary.enable_budget(crate::rrls::budget_negotiation::Profile::MATCHED);
    let shared = Arc::new(Mutex::new(primary));
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut uart = UartComms::test_mesh_pty(port, shared.clone());
    uart.set_side_id(side);
    let tx = uart.try_clone_tx().unwrap().unwrap();
    let state = crate::state::tests::test_app_state().await;
    let workers = spawn_comms_worker_threads(
        router.clone(),
        state.clone(),
        CommsWorkerHandle {
            transport_activity: None,
            name: "rocket_comms",
            tx_budget: Some(budget.clone()),
            comms: Arc::new(Mutex::new(Box::new(uart))),
            tx_comms: Some(Arc::new(Mutex::new(tx))),
            side_id: side,
            tx_rx: receiver,
            legacy_single_worker: false,
            prioritize_rx: false,
            dedicated_radio_io: false,
        },
    )
    .unwrap();
    let mut raw = Vec::new();
    let mut frames = Vec::new();
    let mut cursor = 0;
    let mut first_revoke = None;
    let mut first_chunk = None;
    let mut pending = 0;
    let mut read = [0; 4096];
    for now in (0..=20_000).step_by(5) {
        set_clock(&path, &clock, now);
        if now % 500 == 0 {
            shared.lock().unwrap().incoming(false, &direct_ad, now);
        }
        router.dispatch_tx_queue_with_timeout(0).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        while let Ok(n) = peer.read(&mut read) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&read[..n]);
            assert!(raw.len() < 65536);
        }
        while raw.len() >= cursor + 4 {
            assert_eq!(&raw[cursor..cursor + 2], &[0xa6, 0x5b]);
            let n = usize::from(u16::from_le_bytes(
                raw[cursor + 2..cursor + 4].try_into().unwrap(),
            ));
            if raw.len() < cursor + 4 + n {
                break;
            }
            let body = raw[cursor + 4..cursor + 4 + n].to_vec();
            cursor += 4 + n;
            let is_revoke = body.starts_with(b"RR\x01\x0d") && body.get(5) == Some(&3);
            if is_revoke {
                first_revoke.get_or_insert(now);
            }
            let inner = if body.starts_with(b"RR\x01")
                && matches!(body.get(3), Some(1 | 2 | 3 | 4 | 5 | 6))
            {
                &body[4..body.len() - 4]
            } else {
                body.as_slice()
            };
            if body.starts_with(b"RR\x01") {
                assert_eq!(
                    crc32fast::hash(&body[..body.len() - 4]),
                    u32::from_le_bytes(body[body.len() - 4..].try_into().unwrap())
                );
            }
            if !inner.starts_with(b"RR\x01") {
                strict
                    .rx_packed_from_side(inner, strict_side)
                    .expect("official public434 strict receiver rejects no SDK frame");
                strict.dispatch_tx_queue_with_timeout(0).unwrap();
            }
            let is_chunk = inner.starts_with(b"SDT\x03");
            if is_chunk {
                first_chunk.get_or_insert(now);
            }
            frames.push(serde_json::json!({"at_ms":now,"len":body.len(),"revoke":is_revoke,"sdt_chunk":is_chunk}));
        }
        pending = budget.pending.load(Ordering::Acquire);
        if now > 1200 && pending == 0 && !decoded.lock().unwrap().is_empty() {
            break;
        }
    }
    state.request_shutdown();
    for worker in workers {
        worker.join().unwrap();
    }
    let evidence = path.parent().unwrap();
    std::fs::write(evidence.join("wire.bin"), &raw).unwrap();
    let summary = serde_json::json!({"routing_bytes":routing_bytes,"actual_SDK_catalog_callbacks":accepted.load(Ordering::SeqCst),"first_revoke_ms":first_revoke,"first_catalog_chunk_ms":first_chunk,"pending_bytes":pending,"frames":frames,"scope":"Declared cold workload and actual generic worker/Mesh/UART at32k/15; not captured chronological replay, hardware or ALL-ownerACK proof."});
    std::fs::write(
        evidence.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    println!("COLD_BOOTSTRAP {}", summary);
    assert_eq!(cursor, raw.len(), "no partial physical UART tail");
    assert_eq!(
        pending, 0,
        "all accepted SDK byte owners physically drained"
    );
    assert_eq!(budget.pending_control.load(Ordering::Acquire), 0);
    assert_eq!(budget.pending_ordinary.load(Ordering::Acquire), 0);
    assert!(budget.write_leases.lock().unwrap().is_empty());
    let packets = decoded.lock().unwrap();
    let complete: Vec<_> = packets
        .iter()
        .filter(|p| p.packet_id() == original.packet_id())
        .collect();
    assert_eq!(
        complete.len(),
        1,
        "official SDK must reassemble the original full catalog exactly once"
    );
    assert_eq!(complete[0].payload(), original.payload());
    assert!(
        first_revoke.is_some_and(|at| at <= 500),
        "normal-ready paid bootstrap hidden by cold routing backlog"
    );
    assert!(
        first_chunk.is_none_or(|at| first_revoke.unwrap() < at),
        "bootstrap must precede first new logical catalog prefix"
    );
}

struct PrefixFailureUart {
    uart: UartComms,
    limit: usize,
    failed: bool,
    old_done: bool,
    events: Arc<Mutex<Vec<String>>>,
    event_times: Arc<Mutex<Vec<(String, u64)>>>,
    sender: mpsc::UnboundedSender<(u8, Vec<u8>)>,
    budget: Arc<TxQueueBudget>,
    bulk: Vec<u8>,
    command: Vec<u8>,
}
impl PrefixFailureUart {
    fn record(&self, name: String) {
        self.events.lock().unwrap().push(name.clone());
        self.event_times
            .lock()
            .unwrap()
            .push((name, crate::simulation_clock_ms().unwrap()));
    }
}
impl CommsDevice for PrefixFailureUart {
    fn recv_packet(
        &mut self,
        _: &Router,
        _: &mut dyn FnMut(&Packet),
    ) -> sedsnet::TelemetryResult<()> {
        Ok(())
    }
    fn send_data(
        &mut self,
        payload: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.uart.send_data(payload)
    }
    fn set_side_id(&mut self, side: usize) {
        self.uart.set_side_id(side)
    }
    fn enable_radio_capacity_control(&mut self) {
        self.uart.enable_radio_capacity_control()
    }
    fn service_transmit(&mut self) -> sedsnet::TelemetryResult<()> {
        self.uart.service_transmit()
    }
    fn transmit_ready(&self) -> bool {
        self.uart.transmit_ready()
    }
    fn transmit_ready_for(&self, p: u8, b: &[u8]) -> bool {
        self.uart.transmit_ready_for(p, b)
    }
    fn recovery_budget_configured(&self) -> bool {
        self.uart.recovery_budget_configured()
    }
    fn service_background_transmit_counted(
        &mut self,
        _: bool,
    ) -> sedsnet::TelemetryResult<Option<crate::comms::BackgroundProgress>> {
        Ok(Some(Default::default()))
    }
    fn service_paid_budget_transmit_counted(
        &mut self,
    ) -> sedsnet::TelemetryResult<Option<crate::comms::BackgroundProgress>> {
        // Deliberately keep the protocol unoffered until a raw owner has failed.
        if !self.failed {
            return Ok(None);
        }
        assert!(
            self.old_done,
            "typed protocol cannot run while failed raw owner remains, even after unrelated success"
        );
        let result = self.uart.service_paid_budget_transmit_counted()?;
        if result.as_ref().is_some_and(|p| p.physical_completed) {
            self.record("protocol-full".into());
        }
        Ok(result)
    }
    fn send_tracked_counted(
        &mut self,
        frames: &[(u8, &[u8])],
        leases: &[sedsnet::router::PackedWriteLease],
    ) -> Result<Option<usize>, Box<dyn std::error::Error + Send + Sync>> {
        let old = frames[0].1 == self.bulk;
        if old && !self.failed {
            self.uart.test_scalar_limit(Some(self.limit));
            let result = self.uart.send_tracked_counted(frames, leases);
            let remaining = self.uart.test_scalar_limit(None).unwrap();
            assert_eq!(remaining, 0);
            assert!(result.is_err());
            self.failed = true;
            self.record(format!("old-error-accepted{}", self.limit));
            self.budget
                .try_send(&self.sender, 255, &self.command)
                .unwrap();
            return result;
        }
        let result = self.uart.send_tracked_counted(frames, leases);
        if result.is_ok() {
            if old {
                self.old_done = true;
                self.record("old-full".into());
            } else {
                self.record("unrelated-command-full".into());
            }
        }
        result
    }
}
#[tokio::test]
async fn actual_partial_or_no_write_owner_blocks_protocol_across_unrelated_success() {
    const NAME: &str = "telemetry_task::radio_io::budget_bootstrap_tests::actual_partial_or_no_write_owner_blocks_protocol_across_unrelated_success";
    if std::env::var_os("GS_BUDGET_FAILURE_LIMIT").is_none() {
        for limit in [0, 7] {
            let dir = std::env::temp_dir().join(format!(
                "gs-budget-failure-{}-{limit}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("clock"), "0").unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("GS_BUDGET_FAILURE_LIMIT", limit.to_string())
                .env("GS_SIM_CLOCK_FILE", dir.join("clock"))
                .env("GS_SIMULATED_SERIAL_PTY", "1")
                .env("GS_RRLS_ROLE", "primary")
                .env_remove("GS_RADIO_AIR_SHARE_PERCENT")
                .env_remove("GS_RADIO_AIR_BIT_RATE_BPS")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    panic!("bounded actual prefix child timeout");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let result = child.wait_with_output().unwrap();
            std::fs::write(dir.join("stdout"), &result.stdout).unwrap();
            std::fs::write(dir.join("stderr"), &result.stderr).unwrap();
            println!("FAILURE_OWNER_ARTIFACTS {}", dir.display());
            print!("{}", String::from_utf8_lossy(&result.stdout));
            eprint!("{}", String::from_utf8_lossy(&result.stderr));
            assert!(result.status.success());
        }
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    crate::comms::rfd_profile::initialize(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "failure-private-pty".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    })
    .unwrap();
    let limit = std::env::var("GS_BUDGET_FAILURE_LIMIT")
        .unwrap()
        .parse()
        .unwrap();
    let path = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    let clock = AtomicU64::new(0);
    let ty = sedsnet::config::register_data_type(
        "GS_PHYSICAL_OWNER_64",
        sedsnet::MessageElement::Static(
            64,
            sedsnet::MessageDataType::UInt8,
            sedsnet::MessageClass::Data,
        ),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        sedsnet::ReliableMode::None,
        0,
    )
    .unwrap();
    let bulk = serialize::pack_packet(
        &Packet::new(
            ty,
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            0,
            Arc::from([0x77; 64]),
        )
        .unwrap(),
    )
    .to_vec();
    let command = serialize::pack_packet(
        &Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            "GS",
            0,
            Arc::from([0]),
        )
        .unwrap(),
    )
    .to_vec();
    let (sender, receiver) = mpsc::unbounded_channel();
    let budget = Arc::new(TxQueueBudget::new(32768));
    budget.try_send(&sender, 0, &bulk).unwrap();
    let mut primary = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 42);
    primary.enable_budget(crate::rrls::budget_negotiation::Profile::MATCHED);
    let shared = Arc::new(Mutex::new(primary));
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut uart = UartComms::test_mesh_pty(port, shared);
    uart.set_side_id(0);
    let rx = uart.try_clone_tx().unwrap().unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let expected_bulk = serialize::unpack_packet(&bulk).unwrap();
    let expected_command = serialize::unpack_packet(&command).unwrap();
    let event_times = Arc::new(Mutex::new(Vec::new()));
    let fault = PrefixFailureUart {
        uart,
        limit,
        failed: false,
        old_done: false,
        events: events.clone(),
        event_times: event_times.clone(),
        sender,
        budget: budget.clone(),
        bulk,
        command,
    };
    let state = crate::state::tests::test_app_state().await;
    let workers = spawn_comms_worker_threads(
        Arc::new(Router::new(Default::default())),
        state.clone(),
        CommsWorkerHandle {
            transport_activity: None,
            name: "rocket_comms",
            tx_budget: Some(budget.clone()),
            comms: Arc::new(Mutex::new(rx)),
            tx_comms: Some(Arc::new(Mutex::new(Box::new(fault)))),
            side_id: 0,
            tx_rx: receiver,
            legacy_single_worker: false,
            prioritize_rx: false,
            dedicated_radio_io: false,
        },
    )
    .unwrap();
    let mut raw = Vec::new();
    let mut timed_reads = Vec::<(u64, Vec<u8>)>::new();
    let mut read = [0; 4096];
    for now in (0..=2500).step_by(5) {
        set_clock(&path, &clock, now);
        std::thread::sleep(Duration::from_millis(2));
        while let Ok(n) = peer.read(&mut read) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&read[..n]);
            timed_reads.push((now, read[..n].to_vec()));
            assert!(raw.len() < 16384);
        }
        if events.lock().unwrap().iter().any(|s| s == "protocol-full") {
            break;
        }
    }
    state.request_shutdown();
    for worker in workers {
        worker.join().unwrap();
    }
    std::fs::write(
        path.parent()
            .unwrap()
            .join("wire-with-declared-injected-prefix.bin"),
        &raw,
    )
    .unwrap();
    std::fs::write(path.parent().unwrap().join("timed-wire-observations.json"),serde_json::to_vec_pretty(&serde_json::json!({"reads":timed_reads.iter().map(|(at,b)|serde_json::json!({"at_ms":at,"hex":b.iter().map(|v|format!("{v:02x}")).collect::<String>()})).collect::<Vec<_>>(),"events":&*event_times.lock().unwrap(),"scope":"Actual private PTY read-observation model times, not stop-bit timestamps. Replayed with idle5ms polls; injected prefix retained without manual removal."})).unwrap()).unwrap();
    replay_fault_stream(
        &timed_reads,
        limit,
        &expected_bulk,
        &expected_command,
        path.parent().unwrap(),
    );
    let events = events.lock().unwrap();
    println!("ACTUAL_FAILURE_OWNER {events:?}");
    let position = |name: &str| events.iter().position(|s| s == name).unwrap();
    assert!(position(&format!("old-error-accepted{limit}")) < position("unrelated-command-full"));
    assert!(position("unrelated-command-full") < position("old-full"));
    assert!(position("old-full") < position("protocol-full"));
    assert_eq!(budget.pending.load(Ordering::Acquire), 0);
}

fn replay_fault_stream(
    reads: &[(u64, Vec<u8>)],
    limit: usize,
    bulk: &Packet,
    command: &Packet,
    evidence: &std::path::Path,
) {
    use sedsnet::router::EndpointHandler;
    let received = Arc::new(Mutex::new(Vec::<Packet>::new()));
    let r1 = received.clone();
    let r2 = received.clone();
    let clock = Arc::new(AtomicU64::new(0));
    let tick = clock.clone();
    let strict = Router::new_with_clock(
        RouterConfig::new([
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |p| {
                    r1.lock().unwrap().push(p.clone());
                    Ok(())
                },
            ),
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                move |p| {
                    r2.lock().unwrap().push(p.clone());
                    Ok(())
                },
            ),
        ])
        .with_sender("STRICT-FAULT")
        .with_reliable_enabled(false),
        Box::new(move || tick.load(Ordering::SeqCst)),
    );
    let side = strict.add_side_packed_small_packets("fault-stream", |_| Ok(()), 192);
    let mut decoder = crate::comms::reference_decoder::Decoder::default();
    let mut protocol = 0usize;
    let mut invalid_rr = 0usize;
    let mut sdk_errors = Vec::<String>::new();
    let mut inventory = Vec::new();
    let mut mesh = crate::rrls::Mesh::new(crate::rrls::Role::Rrls, 9);
    let mut consume = |frames: Vec<(bool, Vec<u8>)>| {
        for (command_frame, body) in frames {
            let valid_rr = if body.starts_with(b"RR\x01") {
                let valid = crc32fast::hash(&body[..body.len() - 4])
                    == u32::from_le_bytes(body[body.len() - 4..].try_into().unwrap());
                if !valid {
                    invalid_rr += 1;
                }
                valid
            } else {
                false
            };
            if valid_rr && body.get(3) == Some(&13) {
                assert!(matches!(
                    crate::rrls::budget_negotiation::decode(&body),
                    Some(crate::rrls::budget_negotiation::Message::Revoke(_))
                ));
                protocol += 1;
            }
            inventory.push(serde_json::json!({"at_ms":clock.load(Ordering::SeqCst),"len":body.len(),"rr_prefix":body.starts_with(b"RR\x01"),"rr_kind":body.get(3),"rr_crc_valid":valid_rr,"hex":body.iter().map(|b|format!("{b:02x}")).collect::<String>()}));
            if let Some(frame) = mesh.incoming(command_frame, &body, clock.load(Ordering::SeqCst)) {
                if let Err(error) = strict.rx_packed_from_side(&frame.bytes, side) {
                    sdk_errors.push(format!("{error:?}"));
                }
                strict.dispatch_tx_queue_with_timeout(0).unwrap();
            }
        }
    };
    let mut poll = 0;
    for (at, bytes) in reads {
        while poll < *at {
            clock.store(poll, Ordering::SeqCst);
            consume(decoder.push(&[], poll));
            poll += 5;
        }
        clock.store(*at, Ordering::SeqCst);
        consume(decoder.push(bytes, *at));
    }
    let end = reads.last().map_or(0, |r| r.0) + 5000;
    while poll <= end {
        clock.store(poll, Ordering::SeqCst);
        consume(decoder.push(&[], poll));
        poll += 5;
    }
    drop(consume);
    std::fs::write(evidence.join("actual-parser-receiver-inventory.json"),serde_json::to_vec_pretty(&serde_json::json!({"injected_prefix":limit,"raw_parser_frames":inventory,"raw_rejected":decoder.rejected,"invalid_RR_CRC":invalid_rr,"SDK_errors":sdk_errors,"SDK_packets":received.lock().unwrap().iter().map(|p|serde_json::json!({"packet_id":p.packet_id(),"payload_len":p.payload().len()})).collect::<Vec<_>>(),"protocols":protocol,"tail_bytes":decoder.pending_bytes(),"scope":"Unmodified actual RawUART parser followed by actual Mesh unwrap/rejection and public434 Router; original observed read times +idle polls, no prefix removed."})).unwrap()).unwrap();
    assert_eq!(
        decoder.pending_bytes(),
        0,
        "faultstream tail must resolve through actual parser/idle policy"
    );
    assert_eq!(
        decoder.rejected,
        usize::from(limit > 0),
        "only deliberately injected partial prefix may reject"
    );
    assert_eq!(protocol, 1, "no phantom protocol");
    let packets = received.lock().unwrap();
    assert_eq!(
        packets.len(),
        2,
        "only intended full command and exact retried owner"
    );
    for expected in [bulk, command] {
        let matches: Vec<_> = packets
            .iter()
            .filter(|p| p.packet_id() == expected.packet_id())
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].payload(), expected.payload());
    }
    println!(
        "ACTUAL_FAULT_STRICT_RX acceptedprefix{limit} packets2 protocol1 rejected{} tail0",
        decoder.rejected
    );
}

#[tokio::test]
async fn queued_revoke_follows_completed_health_before_startup_controls() {
    const NAME: &str = "telemetry_task::radio_io::budget_bootstrap_tests::queued_revoke_follows_completed_health_before_startup_controls";
    if std::env::var_os("GS_REVOKE_HEALTH_ORDER_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!(
            "gs-revoke-health-order-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("clock"), "0").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("GS_REVOKE_HEALTH_ORDER_CHILD", "1")
            .env("GS_SIM_CLOCK_FILE", dir.join("clock"))
            .env("GS_SIMULATED_SERIAL_PTY", "1")
            .env("GS_RRLS_ROLE", "primary")
            .env_remove("GS_RADIO_AIR_SHARE_PERCENT")
            .env_remove("GS_RADIO_AIR_BIT_RATE_BPS")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= until {
                child.kill().unwrap();
                panic!("bounded health-order worker timeout");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let result = child.wait_with_output().unwrap();
        std::fs::write(dir.join("stdout"), &result.stdout).unwrap();
        std::fs::write(dir.join("stderr"), &result.stderr).unwrap();
        println!("REVOKE_HEALTH_ARTIFACTS {}", dir.display());
        print!("{}", String::from_utf8_lossy(&result.stdout));
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
        assert!(result.status.success());
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    crate::comms::rfd_profile::initialize(&crate::comms_config::CommsLinkConfig::Serial {
        serial: crate::comms_config::SerialLinkConfig {
            port: "startup-health-pty".into(),
            hardware_flow_control: false,
            rfd_profile: None,
            baud_rate: 115200,
            protocol: crate::comms_config::SerialProtocol::RawUart,
        },
    })
    .unwrap();
    let path = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    let clock = AtomicU64::new(0);
    let mut primary = crate::rrls::Mesh::new(crate::rrls::Role::Primary, 42);
    primary.enable_budget(crate::rrls::budget_negotiation::Profile::MATCHED);
    primary.tick(0);
    // Production enqueue coalesces unserved HEALTH0 when normal tick issues HEALTH1.
    // The already-owned REVOKE was created from HEALTH0 and must not retain seq0.
    let shared = Arc::new(Mutex::new(primary));
    let mut relay = crate::rrls::Mesh::new(crate::rrls::Role::Rrls, 43);
    relay.enable_budget(crate::rrls::budget_negotiation::Profile::MATCHED);
    let relay = Arc::new(Mutex::new(relay));
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut uart = UartComms::test_mesh_pty(port, shared.clone());
    uart.set_side_id(0);
    let tx = uart.try_clone_tx().unwrap().unwrap();
    let (mut relay_peer, relay_port) = serialport::TTYPort::pair().unwrap();
    relay_peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut relay_uart = UartComms::test_mesh_pty(relay_port, relay.clone());
    relay_uart.set_side_id(0);
    let (sender, receiver) = mpsc::unbounded_channel();
    let budget = Arc::new(TxQueueBudget::new(32768));
    let state = crate::state::tests::test_app_state().await;
    set_clock(&path, &clock, 500);
    let workers = spawn_comms_worker_threads(
        Arc::new(Router::new(Default::default())),
        state.clone(),
        CommsWorkerHandle {
            transport_activity: None,
            name: "rocket_comms",
            tx_budget: Some(budget.clone()),
            comms: Arc::new(Mutex::new(Box::new(uart))),
            tx_comms: Some(Arc::new(Mutex::new(tx))),
            side_id: 0,
            tx_rx: receiver,
            legacy_single_worker: false,
            prioritize_rx: false,
            dedicated_radio_io: false,
        },
    )
    .unwrap();
    let mut decoder = crate::comms::reference_decoder::Decoder::default();
    let mut rdecoder = crate::comms::reference_decoder::Decoder::default();
    let mut toward_relay = VecDeque::<(u64, bool, Vec<u8>)>::new();
    let mut toward_primary = VecDeque::<(u64, bool, Vec<u8>)>::new();
    let mut events = Vec::new();
    let mut read = [0; 4096];
    let mut first_health = None;
    let mut first_revoke = None;
    let mut share_before_controls = None;
    let mut submitted = false;
    let mut command_ids = Vec::new();
    let mut command_writes = Vec::new();
    for now in (500..=2200).step_by(5) {
        set_clock(&path, &clock, now);
        while toward_relay.front().is_some_and(|v| v.0 <= now) {
            let (_, command, body) = toward_relay.pop_front().unwrap();
            relay.lock().unwrap().incoming(command, &body, now);
        }
        while toward_primary.front().is_some_and(|v| v.0 <= now) {
            let (_, command, body) = toward_primary.pop_front().unwrap();
            shared.lock().unwrap().incoming(command, &body, now);
        }
        if now == 1495 {
            share_before_controls = shared.lock().unwrap().recovery_air_share(now);
        }
        if now >= 1502 && !submitted {
            for (n, name) in ["FLIGHT_STATE", "AV_BAY_UNDERGLOW", "FLIGHT_BUZZER"]
                .iter()
                .enumerate()
            {
                let packet = Packet::new(
                    crate::telemetry_schema::data_type(name),
                    &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
                    "GS",
                    now,
                    Arc::from([1u8]),
                )
                .unwrap()
                .with_nonce(n as u16 + 1);
                command_ids.push(packet.packet_id());
                budget
                    .try_send(&sender, 255, &serialize::pack_packet(&packet))
                    .unwrap();
            }
            submitted = true;
            events.push(serde_json::json!({"event":"declared_control_cohort","at_ms":now}));
        }
        relay_uart.service_transmit().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        while let Ok(n) = peer.read(&mut read) {
            if n == 0 {
                break;
            }
            for (command, body) in decoder.push(&read[..n], now) {
                if body.starts_with(b"RR\x01\x00") {
                    let seq = u32::from_le_bytes(body[8..12].try_into().unwrap());
                    first_health.get_or_insert(seq);
                    events
                        .push(serde_json::json!({"event":"GS_HEALTH_UART","seq":seq,"at_ms":now}));
                }
                if let Some(crate::rrls::budget_negotiation::Message::Revoke(c)) =
                    crate::rrls::budget_negotiation::decode(&body)
                {
                    first_revoke.get_or_insert(c.seq);
                    events.push(
                        serde_json::json!({"event":"GS_REVOKE_UART","seq":c.seq,"at_ms":now}),
                    );
                }
                let inner = if body.starts_with(b"RR\x01") && matches!(body.get(3), Some(1..=6)) {
                    &body[4..body.len() - 4]
                } else {
                    body.as_slice()
                };
                if let Ok(id) = serialize::packet_id_from_wire(inner) {
                    if command_ids.contains(&id) {
                        command_writes.push((id, now));
                    }
                }
                toward_relay.push_back((now + 64, command, body));
            }
        }
        while let Ok(n) = relay_peer.read(&mut read) {
            if n == 0 {
                break;
            }
            for (command, body) in rdecoder.push(&read[..n], now) {
                if matches!(
                    crate::rrls::budget_negotiation::decode(&body),
                    Some(crate::rrls::budget_negotiation::Message::BaseAck { .. })
                ) {
                    events.push(serde_json::json!({"event":"RRLS_BASE_UART","at_ms":now}));
                }
                toward_primary.push_back((now + 64, command, body));
            }
        }
    }
    state.request_shutdown();
    for worker in workers {
        worker.join().unwrap();
    }
    let summary = serde_json::json!({"first_physical_HEALTH_seq":first_health,"first_physical_REVOKE_seq":first_revoke,"requested_share_at1495":share_before_controls,"declared_control_UART_writes":command_writes,"events":events,"scope":"Actual generic worker/Mesh/UART; declared64ms eachdirection delivery, real normal paidCapacity. Native bootstrap/context proof, not ALLownerMCUACK or hardwarelatency."});
    std::fs::write(
        path.parent().unwrap().join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    println!("REVOKE_HEALTH_ORDER {summary}");
    assert_eq!(decoder.rejected, 0);
    assert_eq!(rdecoder.rejected, 0);
    assert_eq!(first_health, Some(1));
    assert_eq!(
        first_revoke,
        Some(1),
        "unreserved bootstrap must refer to the fully written HEALTH1, not coalesced HEALTH0"
    );
    assert_eq!(
        share_before_controls,
        Some(25),
        "paid BASE proof must arrive before declared1502ms commands"
    );
    assert_eq!(command_writes.len(), 3);
    assert!(command_writes.iter().all(|(_, at)| *at - 1505 <= 500));
    assert_eq!(budget.pending.load(Ordering::Acquire), 0);
}
