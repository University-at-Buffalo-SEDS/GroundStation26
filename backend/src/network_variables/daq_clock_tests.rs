use super::*;

#[test]
fn pending_clock_completion_cannot_erase_a_newer_t0_or_reset() {
    let mut pending = PendingDaqClock::default();
    let first = ([7, 1_800_000_030_000], 10);
    let corrected = ([7, 1_800_000_031_000], 11);
    let reset = ([0, 0], 12);
    pending.record(first.0, first.1);
    pending.record(corrected.0, corrected.1);
    pending.accepted(first);
    assert_eq!(pending.desired, Some(corrected));
    pending.record(reset.0, reset.1);
    pending.accepted(corrected);
    pending.record(first.0, first.1);
    assert_eq!(pending.desired, Some(reset));
    pending.accepted(reset);
    pending.record(corrected.0, corrected.1);
    assert!(pending.desired.is_none());
}

#[test]
fn actual_launch_clock_reaches_daq_after_failed_sdk_admission_without_t0_drift() {
    const NAME: &str = "network_variables::daq_clock_tests::actual_launch_clock_reaches_daq_after_failed_sdk_admission_without_t0_drift";
    if std::env::var_os("GS_DAQ_CLOCK_PROOF_CHILD").is_none() {
        let root =
            std::env::temp_dir().join(format!("gs-daq-clock-proof-{}", rand::random::<u128>()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("clock"), "100000").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("GS_DAQ_CLOCK_PROOF_CHILD", "1")
            .env("GS_DAQ_CLOCK_PROOF_ROOT", &root)
            .env("GS_NETWORK_VARIABLE_CACHE", root.join("variables.json"))
            .env("GS_LAUNCH_CLOCK_STATE", root.join("launch.json"))
            .env("GS_SIM_CLOCK_FILE", root.join("clock"))
            .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000")
            .env("GS_STATION_ROLE", "groundstation")
            .env_remove("GS_RRLS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(proof());
}

async fn proof() {
    use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
    use sedsnet::router::{EndpointHandler, RouterConfig};
    crate::telemetry_schema::initialize().unwrap();
    let root = PathBuf::from(std::env::var_os("GS_DAQ_CLOCK_PROOF_ROOT").unwrap());
    let ty = crate::telemetry_schema::data_type("DAQ_LOG_CLOCK");
    let sd = crate::telemetry_schema::endpoint("SD_CARD");
    let accepted: Arc<Mutex<Vec<([u64; 2], u64)>>> = Default::default();
    let observed = accepted.clone();
    let daq = Arc::new(Router::new(
        RouterConfig::new([EndpointHandler::new_packet_handler(sd, |_| Ok(()))]).with_sender("DAQ"),
    ));
    daq.enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
        .unwrap();
    daq.on_network_variable_update(ty, move |packet| {
        let b = packet.payload();
        observed.lock().unwrap().push((
            [
                u64::from_le_bytes(b[..8].try_into().unwrap()),
                u64::from_le_bytes(b[8..].try_into().unwrap()),
            ],
            packet.timestamp(),
        ));
        Ok(())
    })
    .unwrap();
    let ground = Arc::new(Router::new(RouterConfig::new([]).with_sender("GS")));
    let sent: Arc<Mutex<Vec<Packet>>> = Default::default();
    let wire = sent.clone();
    let remote = Arc::downgrade(&daq);
    ground.add_side_packet("fill", move |packet| {
        if packet.data_type() == ty {
            wire.lock().unwrap().push(packet.clone());
        }
        remote
            .upgrade()
            .map_or(Ok(()), |r| r.rx_from_side(packet, 0))
    });
    let remote = Arc::downgrade(&ground);
    daq.add_side_packet("can", move |packet| {
        remote
            .upgrade()
            .map_or(Ok(()), |r| r.rx_from_side(packet, 0))
    });
    ground
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce("DAQ", 1, &[sd]).unwrap(),
            0,
        )
        .unwrap();
    daq.rx_from_side(
        &sedsnet::discovery::build_discovery_announce(
            "GS",
            1,
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        )
        .unwrap(),
        0,
    )
    .unwrap();
    let state = crate::state::tests::test_app_state().await;
    assert!(state.topology_router.set(ground.clone()).is_ok());
    ground
        .enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
        .unwrap();
    let anchor = crate::telemetry_task::procedure_clock::timestamp_ms();
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    {
        state.begin_daq_launch_log(); // command-acceptance path, before Valve ACK
        assert_eq!(state.launch_clock_snapshot().kind, LaunchClockKind::Idle);
    }
    state.set_launch_clock(crate::state::launch_countdown_clock(anchor));
    let original = persisted_daq_log_clock_with_revision().unwrap();
    assert!(original.0[0] > 0);
    assert_eq!(original.0[1], anchor as u64 + 10_000 + 20_000);
    assert_eq!(PENDING_DAQ_CLOCK.lock().unwrap().desired, Some(original));
    assert!(sent.lock().unwrap().is_empty());
    let notice_count = state.notifications_snapshot().len();
    assert!(retry_pending_daq_log_clock(&ground).is_err());
    assert!(retry_pending_daq_log_clock(&ground).is_err());
    assert_eq!(
        state.notifications_snapshot().len(),
        notice_count,
        "retry failures cannot create a notification storm"
    );
    // An unchanged presentation/clock update must not invent a new deadline.
    fs::write(root.join("clock"), "100833").unwrap();
    state.set_launch_clock(state.launch_clock_snapshot());
    ground
        .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
        .unwrap();
    assert!(retry_pending_daq_log_clock(&ground).unwrap());
    for _ in 0..12 {
        ground.process_all_queues().unwrap();
        daq.process_all_queues().unwrap();
    }
    assert_eq!(accepted.lock().unwrap().last(), Some(&original));
    assert!(PENDING_DAQ_CLOCK.lock().unwrap().desired.is_none());
    let wire = sent.lock().unwrap();
    assert!(!wire.is_empty());
    assert!(
        wire.iter().all(|p| p.endpoints() == [sd]
            && p.timestamp() == original.1
            && p.payload().len() == 16)
    );
    drop(wire);
    let before = sent.lock().unwrap().len();
    for _ in 0..5 {
        assert!(!retry_pending_daq_log_clock(&ground).unwrap());
    }
    assert_eq!(
        sent.lock().unwrap().len(),
        before,
        "stable clock is not periodically refreshed"
    );
    let saved = VariableStore::new(root.join("variables.json"));
    assert_eq!(saved.values.daq_log_clock, original.0);
    assert_eq!(saved.values.daq_log_clock_stamp, original.1);
    let restored = Router::new(RouterConfig::new([]).with_sender("GS"));
    restored
        .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
        .unwrap();
    restored
        .seed_managed_variable(
            daq_log_clock_packet(saved.values.daq_log_clock, saved.values.daq_log_clock_stamp)
                .unwrap(),
        )
        .unwrap();
    let restored_packet = restored.get_cached_network_variable(ty).unwrap().unwrap();
    assert_eq!(restored_packet.timestamp(), original.1);
    assert_eq!(
        restored_packet.payload(),
        daq_log_clock_packet(original.0, original.1)
            .unwrap()
            .payload()
    );
    // Corrected observed T0 keeps this session and gets a newer revision.
    ground
        .enable_network_variable(ty, NetworkVariablePermissions::READ_ONLY)
        .unwrap();
    fs::write(root.join("clock"), "111234").unwrap();
    state.set_launch_clock(LaunchClockMsg {
        kind: LaunchClockKind::TPlus,
        anchor_timestamp_ms: Some(anchor + 11_234),
        duration_ms: None,
    });
    let corrected = persisted_daq_log_clock_with_revision().unwrap();
    assert_eq!(corrected.0[0], original.0[0]);
    assert_eq!(corrected.0[1], original.0[1] + 1234);
    assert!(corrected.1 > original.1);
    assert_eq!(PENDING_DAQ_CLOCK.lock().unwrap().desired, Some(corrected));
    state.try_reset_launch_clock().unwrap();
    let reset = persisted_daq_log_clock_with_revision().unwrap();
    assert_eq!(reset.0, [0, 0]);
    assert!(reset.1 > original.1);
    PENDING_DAQ_CLOCK.lock().unwrap().accepted(corrected); // completion of an older in-flight submit
    assert_eq!(PENDING_DAQ_CLOCK.lock().unwrap().desired, Some(reset));
    ground
        .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
        .unwrap();
    assert!(retry_pending_daq_log_clock(&ground).unwrap());
    // SDK's same-authoritative-sender revision rule must reject a late old submission.
    ground
        .set_network_variable(daq_log_clock_packet(corrected.0, corrected.1).unwrap())
        .unwrap();
    for _ in 0..12 {
        ground.process_all_queues().unwrap();
        daq.process_all_queues().unwrap();
    }
    assert_eq!(accepted.lock().unwrap().last(), Some(&reset));
    DAQ_CLOCK_RETRY_WARNED.store(true, Ordering::Relaxed);
    set_daq_log_clock(&ground, 0, &LaunchClockMsg::idle()).unwrap();
    assert!(!DAQ_CLOCK_RETRY_WARNED.load(Ordering::Relaxed));

    let restart = VariableStore::new(root.join("variables.json"));
    assert_eq!(restart.values.daq_log_clock, reset.0);
    assert_eq!(restart.values.daq_log_clock_stamp, reset.1);
    let restarted = Router::new(RouterConfig::new([]).with_sender("GS"));
    restarted
        .enable_network_variable(ty, NetworkVariablePermissions::READ_WRITE)
        .unwrap();
    restarted
        .seed_managed_variable(
            daq_log_clock_packet(
                restart.values.daq_log_clock,
                restart.values.daq_log_clock_stamp,
            )
            .unwrap(),
        )
        .unwrap();
    let cached = restarted.get_cached_network_variable(ty).unwrap().unwrap();
    assert_eq!(cached.timestamp(), reset.1);
    assert_eq!(cached.payload(), &[0u8; 16]);
    let observer = Router::new(RouterConfig::new([]).with_sender("RRLS"));
    PENDING_DAQ_CLOCK
        .lock()
        .unwrap()
        .record([99, original.0[1]], reset.1 + 1);
    assert!(!retry_pending_daq_log_clock(&observer).unwrap());
    assert_eq!(observer.export_runtime_stats().queues.tx_len, 0);
    println!(
        "DAQ_CLOCK_PROOF original={original:?} reset={reset:?} actual_sdk_packets={} no_periodic_refresh=true",
        sent.lock().unwrap().len()
    );
}
