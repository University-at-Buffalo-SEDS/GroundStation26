// Actual legacy SDK queue-policy and frozen Launch retry controls.
use sedsnet::router::{EndpointHandler, RouterConfig, RouterSideOptions};
use std::sync::{OnceLock, atomic::Ordering};

fn isolated_child() -> bool {
    const CHILD: &str = "GS_LAUNCH_RETRY_CONTROL_CHILD";
    const NAME: &str = "telemetry_task::tests::launch_retry_controls::actual_launch_retry_preserves_originals_stop_and_terminal_prefixes";
    if std::env::var(CHILD).as_deref() == Ok("1") {
        return false;
    }
    struct Owned(std::path::PathBuf);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = Owned(std::env::temp_dir().join(format!(
        "launch-retry-controls-{:032x}",
        rand::random::<u128>()
    )));
    std::fs::create_dir(&root.0).unwrap();
    let config = root.0.join("config");
    std::fs::create_dir(&config).unwrap();
    for name in [
        "flight_setup.json",
        "fill_targets.json",
        "gse_sequence.json",
        "calibration_config.json",
        "station_positions.json",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config")
            .join(name);
        if path.exists() {
            std::fs::copy(path, config.join(name)).unwrap();
        }
    }
    let out = root.0.join("stdout.log");
    let err = root.0.join("stderr.log");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .env("TMPDIR", &root.0)
        .env("TMP", &root.0)
        .env("TEMP", &root.0)
        .env(
            "GS_NETWORK_VARIABLE_CACHE",
            root.0.join("network-variables.json"),
        )
        .env("GS_LOADCELL_CALIBRATION_PATH", root.0.join("loadcell.json"))
        .env("GS_CHECKLISTS_FILE", root.0.join("checklists.json"))
        .env("GS_FLIGHT_SETUP_CONFIG", config.join("flight_setup.json"))
        .env("GS_FILL_TARGETS_CONFIG", config.join("fill_targets.json"))
        .env("GS_GSE_CONFIG", config.join("gse_sequence.json"))
        .env("GS_AUTO_ZERO_CONFIG", config.join("auto_zero.json"))
        .env(
            "GS_CALIBRATION_CONFIG_PATH",
            config.join("calibration_config.json"),
        )
        .env(
            "GS_STATION_POSITIONS_PATH",
            config.join("station_positions.json"),
        )
        .env("GS_GSE_QUALIFICATION", config.join("qualification.json"))
        .env("GS_ZERO_CAPTURE_DIR", root.0.join("zero-captures"))
        .stdout(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out)
                .unwrap(),
        )
        .stderr(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&err)
                .unwrap(),
        )
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut exceeded = false;
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline
            || [&out, &err]
                .into_iter()
                .any(|p| std::fs::metadata(p).unwrap().len() > 1_048_576)
        {
            exceeded = true;
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait().unwrap();
    let read = |p: &std::path::Path| {
        use std::io::Read;
        let mut data = Vec::new();
        std::fs::File::open(p)
            .unwrap()
            .take(1_048_577)
            .read_to_end(&mut data)
            .unwrap();
        assert!(data.len() <= 1_048_576);
        String::from_utf8_lossy(&data).into_owned()
    };
    let text = format!("{}{}", read(&out), read(&err));
    assert!(!exceeded, "owned control child exceeded bounds:{text}");
    assert!(status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
    println!("{text}");
    true
}

struct Rig {
    state: Arc<AppState>,
    host: Arc<Router>,
    peers: Vec<Arc<Router>>,
    wire: Arc<Mutex<Vec<Packet>>>,
    delivered: Arc<Mutex<Vec<Packet>>>,
    _db: mpsc::Receiver<DbQueueItem>,
}
impl Rig {
    async fn new() -> Self {
        crate::telemetry_schema::initialize().unwrap();
        let (tx, rx) = mpsc::channel(64);
        let state = test_app_state(tx).await;
        let owner = Arc::as_ptr(&state) as usize;
        let start = std::time::Instant::now();
        let host = Arc::new(Router::new_with_clock(
            RouterConfig::new([]).with_sender("GS"),
            Box::new(move || {
                crate::gse::nonwaiting_admission_tests::clock_pause(owner);
                let elapsed = start.elapsed();
                crate::gse::nonwaiting_admission_tests::record_holder_clock_return(
                    owner, start + elapsed, elapsed.as_millis() as u64,
                );
                elapsed.as_millis() as u64
            }),
        ));
        refresh_host_network_time(&host, get_current_timestamp_ms());
        state.topology_router.set(host.clone()).unwrap();
        let wire = Arc::new(Mutex::new(Vec::new()));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut peers = Vec::new();
        for (name, endpoints) in [
            (
                "FC",
                vec![
                    crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER"),
                    crate::telemetry_schema::endpoint("FLIGHT_STATE"),
                ],
            ),
            ("VB", vec![crate::telemetry_schema::endpoint("VALVE_BOARD")]),
        ] {
            let handlers = endpoints
                .into_iter()
                .map(|ep| {
                    let seen = delivered.clone();
                    EndpointHandler::new_packet_handler(ep, move |p| {
                        seen.lock().unwrap().push(p.clone());
                        Ok(())
                    })
                })
                .collect::<Vec<_>>();
            let peer = Arc::new(Router::new(RouterConfig::new(handlers).with_sender(name)));
            let host_side = Arc::new(OnceLock::new());
            let side_copy = host_side.clone();
            let receiver = Arc::downgrade(&host);
            let peer_side = peer.add_side_packed_with_options(
                "GS",
                move |bytes| {
                    receiver
                        .upgrade()
                        .unwrap()
                        .rx_packed_from_side(bytes, *side_copy.get().unwrap())
                },
                RouterSideOptions {
                    reliable_enabled: false,
                    ..Default::default()
                },
            );
            let receiver = peer.clone();
            let captured = wire.clone();
            let side = host.add_side_packed_with_options(
                name,
                move |bytes| {
                    if let Ok(p) = serialize::unpack_packet(bytes) {
                        captured.lock().unwrap().push(p);
                    }
                    receiver.rx_packed_from_side(bytes, peer_side)
                },
                RouterSideOptions {
                    reliable_enabled: false,
                    ..Default::default()
                },
            );
            host_side.set(side).unwrap();
            peer.announce_discovery().unwrap();
            peers.push(peer);
        }
        let mut rig = Self {
            state,
            host,
            peers,
            wire,
            delivered,
            _db: rx,
        };
        rig.pump();
        rig
    }
    fn pump(&mut self) {
        for _ in 0..32 {
            self.host.process_all_queues_with_timeout(0).unwrap();
            for peer in &self.peers {
                peer.process_all_queues_with_timeout(0).unwrap();
            }
        }
    }
    fn ticket(&self) -> crate::gse::LaunchAdmission {
        crate::gse::qualified_launch_proofs_for_test(&self.state);
        crate::gse::inhibit_automatic_actions_for_launch(&self.state).unwrap()
    }
}

async fn actual_busy(kind: &str) {
    let mut rig = Rig::new().await;
    let ticket = rig.ticket();
    let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&rig.state);
    let trace = pause.trace.clone();
    trace.enable(kind == "transient");
    if kind == "new_fc16" {
        pause.hold_retry();
    }
    let packets = vec![
        controller_packet(
            &rig.host,
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[1],
        )
        .unwrap(),
        controller_packet(
            &rig.host,
            crate::telemetry_schema::data_type("VALVE_COMMAND"),
            &[6],
        )
        .unwrap(),
    ];
    let state = rig.state.clone();
    let router = rig.host.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let caller_trace = trace.clone();
    let caller = std::thread::spawn(move || {
        let result = crate::gse::queue_admitted_launch_packets(
            &state, &router, ticket, packets,
        );
        caller_trace.mark(crate::gse::nonwaiting_admission_tests::TraceEvent::CallerReturned,
            result.as_ref().map_or(u64::MAX, |n| *n as u64));
        tx.send(result).unwrap();
    });
    pause.prepared.recv_timeout(Duration::from_secs(2)).unwrap();
    let originals = pause.launch_packets.lock().unwrap().clone();
    let expected = if cfg!(feature = "hitl_mode") { 2 } else { 3 };
    assert_eq!(originals.len(), expected);
    let seed = Packet::new(
        crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "GS",
        100,
        Arc::from([0u8]),
    )
    .unwrap();
    let host = rig.host.clone();
    use crate::gse::nonwaiting_admission_tests::TraceEvent;
    trace.mark(TraceEvent::HolderSpawnRequested, 0);
    let holder_trace = trace.clone();
    let holder = std::thread::Builder::new()
        .name("production-sdk-core-holder".into())
        .spawn(move || {
            holder_trace.mark(TraceEvent::HolderStarted, 0);
            let result = host.seed_managed_variable(seed);
            holder_trace.mark(TraceEvent::HolderCompleted, u64::from(result.is_ok()));
            result
        })
        .unwrap();
    pause
        .core_entered
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    trace.mark(TraceEvent::CoreEnteredObserved, 0);
    pause.proceed.send(()).unwrap();
    pause
        .busy
        .recv_timeout(Duration::from_millis(100))
        .expect("actual firstzero-owner CoreBusy");
    trace.mark(TraceEvent::BusyObserved, 0);
    let result = match kind {
        "transient" => {
            trace.mark(TraceEvent::Sleep20Started, 0);
            std::thread::sleep(Duration::from_millis(20));
            trace.mark(TraceEvent::Sleep20Returned, 0);
            trace.mark(TraceEvent::ReleaseRequested, 0);
            pause.release();
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }
        "stop" => {
            let stop = rig.state.manual_abort_intent.clone();
            let (tx, retained) = std::sync::mpsc::channel();
            let requester = std::thread::spawn(move || tx.send(stop.request_untimed()).unwrap());
            assert_eq!(
                retained
                    .recv_timeout(Duration::from_millis(100))
                    .expect("real Stop retention beforeCore release"),
                1
            );
            requester.join().unwrap();
            pause.release();
            rx.recv_timeout(Duration::from_secs(2)).unwrap()
        }
        "new_fc16" => {
            pause.release_core();
            let fc16 = controller_packet(
                &rig.host,
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &[16],
            )
            .unwrap();
            let fc16_id = fc16.packet_id();
            rig.host.tx_queue(fc16).unwrap();
            assert!(
                rig.host
                    .has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                        "FLIGHT_COMMAND"
                    )),
                "actual publicSDK retains newFC16 beforeoldretry resumes"
            );
            pause.resume_retry();
            let result = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(
                result
                    .as_ref()
                    .unwrap_err()
                    .contains("Controller command delivery is pending")
            );
            assert_eq!(
                rig.state.manual_abort_intent.epoch(),
                0,
                "FC16 doesnot fabricate manualStop epoch"
            );
            eprintln!(
                "NEW_FC16_DURING_RETRY: actual_owner_id={fc16_id} original_sdk_call_count={} old_launch_refused=true",
                pause.launch_calls.load(Ordering::Relaxed)
            );
            result
        }
        "permanent" => {
            let result = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("new100msbudget ends whileCorestillheld");
            pause.release();
            result
        }
        _ => unreachable!(),
    };
    holder.join().unwrap().unwrap();
    caller.join().unwrap();
    let calls = pause.launch_calls.load(Ordering::Relaxed);
    drop(pause);
    assert!(
        rig.state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    if kind == "transient" {
        // Read/format only AFTER the original holder and caller joins and
        // inhibition assertion. No SDK/state owner is held by the ledger.
        let error = result.as_ref().err().map(|text| text.chars().take(512).collect::<String>());
        let context = serde_json::json!({
            "kind":kind,"expected_originals":expected,"sdk_calls":calls,
            "result_ok_count":result.as_ref().ok(),"error_preview":error,
            "original_retry_window_ms":100,"inhibition_assertion_already_passed":true,
            "originals":originals.iter().map(|p| serde_json::json!({
                "packet_id":p.packet_id(),"source_utc":p.timestamp(),"type":p.data_type().as_u32()
            })).collect::<Vec<_>>(),"timeline":trace.snapshot(),
            "provenance":"Diagnostic wall observations, not SDK delivery/physical output proof; no clock or intent restamp"
        });
        let encoded = serde_json::to_string(&context).unwrap();
        if encoded.len() <= 32_768 {
            eprintln!("LAUNCH_TRANSIENT_TIMELINE_DIAGNOSTIC {encoded}");
        } else {
            eprintln!("LAUNCH_TRANSIENT_TIMELINE_DIAGNOSTIC format_cap_exceeded=true context_not_emitted=true");
        }
        assert_eq!(result.unwrap(), expected);
        assert!(
            calls > expected,
            "actualCoreBusy retried beforeoriginalsuccess"
        );
        rig.pump();
        let delivered = rig.delivered.lock().unwrap();
        let wire = rig.wire.lock().unwrap();
        for original in &originals {
            let matching = delivered
                .iter()
                .filter(|p| p.packet_id() == original.packet_id())
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1, "one peerapplicationdelivery peroriginal");
            assert_eq!(matching[0].timestamp(), original.timestamp());
            assert_eq!(matching[0].payload(), original.payload());
            let actual = wire
                .iter()
                .filter(|p| p.packet_id() == original.packet_id())
                .collect::<Vec<_>>();
            assert!(!actual.is_empty());
            assert!(actual.iter().all(|p| p.timestamp() == original.timestamp()));
        }
    } else {
        let error = result.unwrap_err();
        if kind == "permanent" {
            assert!(error.contains("accepted 0/") && error.contains("router core busy"));
            assert_eq!(
                calls, 1,
                "nonwaiting pending inspector prevents SDK retry while Core stays busy"
            );
        } else if kind == "new_fc16" {
            assert!(error.contains("Controller command delivery is pending"));
            assert_eq!(calls, 1, "typed pendingguard refusesbefore any oldSDKretry");
        } else {
            assert!(error.contains("Manual Abort"));
            assert_eq!(rig.state.manual_abort_intent.epoch(), 1);
        }
        rig.pump();
        assert!(!rig.wire.lock().unwrap().iter().any(|p| {
            originals
                .iter()
                .any(|original| original.packet_id() == p.packet_id())
        }));
    }
    eprintln!(
        "LAUNCH_RETRY_ACTUAL: kind={kind} expected_originals={expected} sdk_calls={calls} originals={:?}",
        originals
            .iter()
            .map(|p| (p.packet_id(), p.timestamp()))
            .collect::<Vec<_>>()
    );
    rig.state.request_shutdown();
}

async fn queue_policy(case: &str) {
    crate::telemetry_schema::initialize().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let state = test_app_state(tx).await;
    let memory = sedsnet::config::RuntimeMemoryConfig::new(1_048_576, 8, 1, 1.0).unwrap();
    let host = Arc::new(Router::new(
        RouterConfig::new([])
            .with_sender("GS")
            .with_memory_config(memory)
            .unwrap(),
    ));
    refresh_host_network_time(&host, get_current_timestamp_ms());
    state.topology_router.set(host.clone()).unwrap();
    crate::gse::qualified_launch_proofs_for_test(&state);
    let ticket = crate::gse::inhibit_automatic_actions_for_launch(&state).unwrap();
    if case == "capacity" {
        let ep = sedsnet::config::register_endpoint_with_description(
            "LAUNCH_RETRY_CAPACITY_OWNER",
            "",
            false,
        )
        .unwrap();
        let ty = sedsnet::config::register_data_type(
            "LAUNCH_RETRY_CAPACITY_VALUE",
            sedsnet::MessageElement::Static(
                1,
                sedsnet::MessageDataType::UInt8,
                sedsnet::MessageClass::Data,
            ),
            &[ep],
            sedsnet::ReliableMode::Ordered,
            254,
        )
        .unwrap();
        host.try_tx_queue(Packet::new(ty, &[ep], "GS", 100, Arc::from([0u8])).unwrap())
            .unwrap();
    }
    let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&state);
    let packets = if case == "empty" {
        Vec::new()
    } else {
        vec![
            controller_packet(
                &host,
                crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
                &[1],
            )
            .unwrap(),
            controller_packet(
                &host,
                crate::telemetry_schema::data_type("VALVE_COMMAND"),
                &[6],
            )
            .unwrap(),
        ]
    };
    let s = state.clone();
    let r = host.clone();
    let worker = std::thread::spawn(move || {
        crate::gse::queue_prepared_launch_packets(&s, &r, ticket, packets, None)
    });
    pause.prepared.recv_timeout(Duration::from_secs(2)).unwrap();
    pause.proceed.send(()).unwrap();
    let result = worker.join().unwrap();
    let calls = pause.launch_calls.load(Ordering::Relaxed);
    drop(pause);
    match case {
        "capacity" => {
            let error = result.unwrap_err();
            assert!(
                error.contains("accepted 0/2") && error.contains("priority queue saturated"),
                "{error}"
            );
            assert_eq!(calls, 1);
            assert_eq!(host.export_runtime_stats().queues.tx_len, 1);
        }
        "partial" => {
            let error = result.unwrap_err();
            assert!(
                error.contains("accepted 1/2") && error.contains("priority queue saturated"),
                "{error}"
            );
            assert_eq!(calls, 2);
            assert_eq!(host.export_runtime_stats().queues.tx_len, 1);
            assert!(
                host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                    "FLIGHT_COMMAND"
                ))
            );
            assert!(
                !host.has_pending_reliable_tx_type(crate::telemetry_schema::data_type(
                    "VALVE_COMMAND"
                ))
            );
        }
        "empty" => {
            assert_eq!(result.unwrap(), 0);
            assert_eq!(calls, 0);
            assert_eq!(host.export_runtime_stats().queues.tx_len, 0);
        }
        _ => unreachable!(),
    }
    assert!(
        state
            .launch_history
            .lock()
            .unwrap()
            .automatic_actions_inhibited()
    );
    eprintln!("LAUNCH_QUEUE_POLICY: case={case} sdk_calls={calls}");
    state.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_launch_retry_preserves_originals_stop_and_terminal_prefixes() {
    if isolated_child() {
        return;
    }
    for case in ["transient", "permanent", "stop", "new_fc16"] {
        actual_busy(case).await;
    }
    for case in ["capacity", "partial", "empty"] {
        queue_policy(case).await;
    }
}
