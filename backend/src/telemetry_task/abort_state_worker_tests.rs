//! The radio worker and mesh path with the station's air turns: an operator
//! Abort (FLIGHT_STATE=15 then ABORT, as the station queues them) written
//! while RF holds the air must put FLIGHT_STATE=15 on the wire with the
//! ABORT, or a few ms after it, never behind the next turn gap.
use super::*;
use sedsnet::router::{EndpointHandler, RetryTrackerLimits, RouterConfig, RouterSideOptions};
use serialport::SerialPort;
use std::io::Read;
use std::sync::atomic::AtomicU64;

fn frames(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut cursor = 0;
    let mut out = Vec::new();
    while raw.len() - cursor >= 4 {
        let length = u16::from_le_bytes(raw[cursor + 2..cursor + 4].try_into().unwrap()) as usize;
        let end = cursor + 4 + length;
        if end > raw.len() {
            break;
        }
        out.push(raw[cursor + 4..end].to_vec());
        cursor = end;
    }
    out
}

/// The SDK frames an RR AV_BAY_DATA envelope carries (a compact bundle is
/// expanded).
fn sdk_frames(body: &[u8]) -> Vec<Vec<u8>> {
    if !(body.starts_with(b"RR\x01") && body.len() >= 8 && matches!(body[3], 2 | 5)) {
        return Vec::new();
    }
    let inner = &body[4..body.len() - 4];
    if sedsnet::wire_bundle::is_bundle(inner) {
        sedsnet::wire_bundle::unpack_bundle_frames(inner).unwrap_or_default()
    } else {
        vec![inner.to_vec()]
    }
}

#[tokio::test]
async fn an_abort_in_rf_burst_takes_its_flight_state_along() {
    if !cfg!(feature = "firmware_sim") {
        crate::mode_suite!(Hitl);
    }
    const NAME: &str = "telemetry_task::radio_io::abort_state_worker_tests::an_abort_in_rf_burst_takes_its_flight_state_along";
    const CHILD: &str = "GS_ABORT_STATE_WORKER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = std::env::temp_dir().join(format!("gs-abort-state-worker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clock"), "30000").unwrap();
        let variant = std::env::var("GS_ABORT_STATE_VARIANT").unwrap_or_else(|_| "urgent".into());
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, variant)
            .env("GS_SIM_CLOCK_FILE", dir.join("clock"))
            .env("GS_SIMULATED_SERIAL_PTY", "1")
            .env("GS_RRLS_ROLE", "primary")
            .env("GS_NETWORK_VARIABLE_CACHE", dir.join("network.json"))
            .env_remove("GS_RADIO_TURN_TAKING")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        print!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    crate::telemetry_schema::initialize().unwrap();
    let clock = std::path::PathBuf::from(std::env::var_os("GS_SIM_CLOCK_FILE").unwrap());
    let now = Arc::new(AtomicU64::new(30_000));
    let sdk_now = now.clone();
    let set_now = |ms: u64| {
        now.store(ms, Ordering::SeqCst);
        std::fs::write(&clock, ms.to_string()).unwrap();
    };
    let router = Arc::new(Router::new_with_clock(
        RouterConfig::new([EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint("GROUND_STATION"),
            |_| Ok(()),
        )])
        .with_sender("GS"),
        Box::new(move || sdk_now.load(Ordering::SeqCst)),
    ));
    let variant = std::env::var(CHILD).unwrap();
    if variant != "plain" {
        for name in ["ABORT", "FLIGHT_COMMAND"] {
            sedsnet::set_type_scheduling_class(crate::telemetry_schema::data_type(name), -1);
        }
    }
    router
        .enable_network_variable(
            crate::telemetry_schema::data_type("FLIGHT_STATE"),
            sedsnet::router::NetworkVariablePermissions::READ_WRITE,
        )
        .unwrap();
    let budget = Arc::new(TxQueueBudget::new_radio(2048, 256));
    let (sender, receiver) = mpsc::unbounded_channel();
    let queue_budget = budget.clone();
    let activity: Arc<TransportTxActivity> = Arc::new(Default::default());
    let side_activity = activity.clone();
    let (side, tracker) = router
        .add_side_packed_leased_with_priority_and_options(
            "rocket_comms",
            move |lease, priority| queue_budget.try_send_leased_tracked(&sender, priority, lease, &side_activity),
            RouterSideOptions { reliable_enabled: false, ..Default::default() }
                .with_small_packet_transport(192),
            RetryTrackerLimits::default(),
        )
        .unwrap();
    let mesh = Arc::new(Mutex::new(crate::rrls::Mesh::new(crate::rrls::Role::Primary, 42)));
    let (mut peer, port) = serialport::TTYPort::pair().unwrap();
    peer.set_timeout(Duration::from_millis(1)).unwrap();
    let mut rx = crate::comms::UartComms::test_mesh_pty(port, mesh.clone());
    rx.set_side_id(side);
    rx.bind_retry_tracker(tracker).unwrap();
    rx.enable_turn_gate_for_test(15);
    let tx = rx.try_clone_tx().unwrap().unwrap();
    let state = crate::state::tests::test_app_state().await;
    let _workers = spawn_comms_worker_threads(
        router.clone(),
        state,
        CommsWorkerHandle {
            transport_activity: Some(activity.clone()),
            name: "rocket_comms",
            tx_budget: Some(budget),
            comms: Arc::new(Mutex::new(Box::new(rx))),
            tx_comms: Some(Arc::new(Mutex::new(tx))),
            side_id: side,
            tx_rx: receiver,
            legacy_single_worker: false,
            prioritize_rx: false,
            dedicated_radio_io: false,
        },
    )
    .unwrap();
    // RF's turn: 90 ms of air at the start of every 200 ms period.
    let in_burst = |ms: u64| ms % 200 < 90;
    let pack = |name: &str, endpoint: &str, payload: &[u8]| {
        Packet::new(
            crate::telemetry_schema::data_type(name),
            &[crate::telemetry_schema::endpoint(endpoint)],
            "GS",
            1_767_225_600_000,
            Arc::from(payload),
        )
        .unwrap()
    };
    let mut raw = Vec::new();
    let mut seen: Vec<(u64, Vec<Vec<u8>>)> = Vec::new();
    let mut parsed = 0;
    let mut abort_at = None;
    let mut buf = [0u8; 4096];
    for ms in 30_000u64..33_500 {
        set_now(ms);
        if in_burst(ms) {
            crate::comms::note_air_rx_now();
            mesh.lock().unwrap().incoming(true, b"RS\x01\x00\x00\x00\x00\x00\x00", ms);
        }
        // An operator Abort well inside RF's burst, once HEALTH has a turn
        // history: FLIGHT_STATE=15 is queued first, then the ABORT, and the
        // station flushes the SDK at once (publish_local_claim).
        if ms == 32_430 {
            assert!(in_burst(ms));
            // As the station: set_network_variable (sent at once, or queued
            // while another thread is in side TX), then the ABORT queued and
            // the SDK flushed for 3 ms.
            router.set_network_variable(pack("FLIGHT_STATE", "FLIGHT_STATE", &[15])).unwrap();
            router.tx_queue(pack("ABORT", "ABORT", b"SEDS_MANUAL_ABORT_V1:K")).unwrap();
            router.process_all_queues_with_timeout(3).unwrap();
        }
        // Ordinary station telemetry keeps the router's side busy, as the
        // router worker does on the station.
        if ms % 20 == 0 {
            let _ = router.tx_queue(pack("BATTERY_VOLTAGE", "GROUND_STATION", &12.5f32.to_le_bytes()));
        }
        // Only the RX-driven router pass dispatches: the station's router
        // worker runs when it receives (RF's burst), not on a timer.
        if in_burst(ms) {
            router.dispatch_tx_queue_with_timeout(0).unwrap();
        }
        std::thread::sleep(Duration::from_millis(1));
        while let Ok(n) = peer.read(&mut buf) {
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        let all = frames(&raw);
        for body in &all[parsed..] {
            seen.push((ms, sdk_frames(body)));
        }
        parsed = all.len();
        if abort_at.is_some_and(|at| ms > at + 400) {
            break;
        }
        if abort_at.is_none()
            && seen.iter().any(|(_, sdk)| sdk.iter().any(|f| {
                serialize::peek_frame_info_unchecked(f)
                    .is_ok_and(|i| i.envelope.ty == crate::telemetry_schema::data_type("ABORT"))
            }))
        {
            abort_at = Some(ms);
        }
    }
    let first = |ty: &str, payload: Option<&[u8]>| {
        seen.iter().find_map(|(ms, sdk)| {
            sdk.iter()
                .any(|f| {
                    serialize::unpack_packet(f).is_ok_and(|p| {
                        p.data_type() == crate::telemetry_schema::data_type(ty)
                            && payload.is_none_or(|v| p.payload() == v)
                    })
                })
                .then_some(*ms)
        })
    };
    let abort = first("ABORT", None).expect("the ABORT reached the wire");
    let state = first("FLIGHT_STATE", Some(&[15])).expect("FLIGHT_STATE=15 reached the wire");
    println!("ABORT_STATE_WORKER abort_ms={abort} state_ms={state} lag_ms={}", state as i64 - abort as i64);
    assert!(abort <= 32_436, "the ABORT goes at once, out of turn (at {abort})");
    assert!(
        state <= abort + 5,
        "FLIGHT_STATE=15 left {} ms after its ABORT (the next gap is at 32_490+)",
        state as i64 - abort as i64
    );
}
