// Controlled-clock, actual packet-handler regression. No hardware transport is attached.
use super::*;

/// Use the same authenticated route and sole SQLite owner as production Fill.
/// The packet-handler DB channel below stays independently blocked on purpose.
async fn arm_owned_fill_capture(
    state: &mut Arc<AppState>,
    directory: &std::path::Path,
) -> super::recording_owner::TestOwner {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let users = directory.join("fill-users.json");
    std::fs::write(&users, serde_json::to_vec(&serde_json::json!({
        "version": 1, "users": [{"username": "zero-operator", "password": {"salt_b64":"", "hash_b64":""},
        "roles": [], "permissions": {"view_data":true,"send_commands":true},
        "command_access": {"allowed_commands":["StartFill"]}}]
    })).unwrap()).unwrap();
    Arc::get_mut(state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users));
    crate::ensure_auth_sessions_table(&state.auth_db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('zero-token','zero-operator','session',1,1,0,9999999999999)")
        .execute(&state.auth_db).await.unwrap();
    let owner = super::recording_owner::TestOwner::start(state, &directory.join("fill-recordings"));
    state.gse.lock().unwrap().observe_mass(Some(0.));
    state.gse.lock().unwrap().observe_pressure(Some(0.));
    let app = crate::web::router(state.clone(), "zero-fixture-video-password".into());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/command")
                .header("content-type", "application/json")
                .header("authorization", "Bearer zero-token")
                .body(Body::from("\"StartFill\""))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        state.recording_status_snapshot().mode,
        crate::telemetry_db::RecordingModeWire::Recording
    );
    assert_ne!(state.fill_recording.ready_instance(), 0);
    assert!(state.fill_recording.continuation.lock().unwrap().is_some());
    assert!(state.auto_zero.lock().unwrap().pending());
    owner
}

fn isolated_zero_case(case: &str, test_name: &str) {
    let child_case = std::env::var("GS_ZERO_INGRESS_CHILD");
    assert!(
        !matches!(child_case.as_deref(),Ok(child) if child!=case),
        "isolated child case mismatch must fail, never recursively spawn"
    );
    if child_case.as_deref() == Ok(case) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(zero_case(case));
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "gs-zero-ingress-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("clock"), "100000").unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env("GS_ZERO_INGRESS_CHILD", case)
        .env("GS_ZERO_INGRESS_ROOT", &dir)
        .env("GS_SIM_CLOCK_FILE", dir.join("clock"))
        .env("GS_SIM_NETWORK_EPOCH_MS", "1767225600000")
        .env_remove("GS_STATION_PAIRING_FILE");
    for (name, file) in [
        ("GS_AUTO_ZERO_CONFIG", "zero.json"),
        ("GS_LOADCELL_CALIBRATION_PATH", "calibration.json"),
        ("GS_CALIBRATION_CONFIG_PATH", "calibration-config.json"),
        ("GS_NETWORK_VARIABLE_CACHE", "variables.json"),
        ("GS_GSE_CONFIG", "gse.json"),
        ("GS_GSE_QUALIFICATION", "qualification.json"),
        ("GS_FILL_TARGETS_CONFIG", "fill.json"),
        ("GS_FLIGHT_SETUP_CONFIG", "flight.json"),
        ("GS_CHECKLISTS_FILE", "checklists.json"),
    ] {
        command.env(name, dir.join(file));
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut exceeded = false;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            exceeded = true;
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !exceeded,
        "isolated fixture exceeded its10-second bound and was terminated"
    );
    let _ = std::fs::remove_dir_all(&dir);
    if case.starts_with("recorded_") || case == "kg50" {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn loadcell_zero_capture_precedes_recording_backpressure() {
    isolated_zero_case(
        "blocked",
        "telemetry_task::tests::zero_ingress::loadcell_zero_capture_precedes_recording_backpressure",
    );
}
#[test]
fn loadcell_zero_capture_has_margin_for_measured_3986_hz() {
    isolated_zero_case(
        "cadence",
        "telemetry_task::tests::zero_ingress::loadcell_zero_capture_has_margin_for_measured_3986_hz",
    );
}

#[test]
fn combined_loadcell_pressure_packet_counts_only_one_stability_sample() {
    isolated_zero_case(
        "combined",
        "telemetry_task::tests::zero_ingress::combined_loadcell_pressure_packet_counts_only_one_stability_sample",
    );
}
#[test]
fn selected_kg50_capture_precedes_recording_backpressure() {
    isolated_zero_case(
        "kg50",
        "telemetry_task::tests::zero_ingress::selected_kg50_capture_precedes_recording_backpressure",
    );
}
#[test]
fn replayed_packet_cannot_satisfy_minimum_stability_samples() {
    isolated_zero_case(
        "replay",
        "telemetry_task::tests::zero_ingress::replayed_packet_cannot_satisfy_minimum_stability_samples",
    );
}
#[test]
fn prewindow_input_does_not_become_fresh_when_recording_unblocks() {
    isolated_zero_case(
        "prewindow",
        "telemetry_task::tests::zero_ingress::prewindow_input_does_not_become_fresh_when_recording_unblocks",
    );
}

async fn zero_case(case: &str) {
    let epoch = if matches!(case, "recorded_load" | "recorded_launch") {
        let fixture: serde_json::Value = serde_json::from_str(if case == "recorded_launch" {
            include_str!("../auto_zero/fixtures/incident-2026-10-31-kg1000.json")
        } else {
            include_str!("../auto_zero/fixtures/incident-2026-10-31-kg50.json")
        })
        .unwrap();
        fixture["samples"][0]["timestamp_ms"].as_u64().unwrap()
    } else {
        1767225700000
    };
    let clock_router = Arc::new(Router::new_with_clock(
        sedsnet::router::RouterConfig::new([]).with_sender("GS"),
        Box::new(|| crate::simulation_clock_ms().unwrap()),
    ));
    clock_router.set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(epoch));
    set_network_time_router(clock_router);
    if matches!(case, "foreign" | "stale_source") {
        rejected_source_case(case).await;
        return;
    }
    if matches!(case, "recorded_load" | "recorded_launch") {
        recorded_load_case(case == "recorded_launch").await;
        return;
    }

    if case == "prewindow" {
        prewindow_case().await;
        return;
    }
    let kg50 = case == "kg50";
    let blocked = case != "cadence";

    let dir = std::path::PathBuf::from(std::env::var_os("GS_ZERO_INGRESS_ROOT").unwrap());
    let (db_tx, mut db_rx) = mpsc::channel(16);
    let mut state = if kg50 {
        crate::gse::fill_recording_test_state(0., gse_sequence::Phase::Passed, None)
            .await
            .0
    } else {
        test_app_state(db_tx.clone()).await
    };
    let mut cfg = loadcell::LoadcellCalibrationFile::default();
    cfg.ch1.m = Some(1.0);
    cfg.ch1.b = Some(0.0);
    cfg.ch1_zero_raw = None;
    if kg50 {
        let channel = cfg.extra_channels.entry("kg50".into()).or_default();
        channel.linear.m = Some(1.0);
        channel.linear.b = Some(0.0);
    }
    *state.loadcell_calibration.lock().unwrap() = cfg;
    *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    let _fill_owner = if kg50 {
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg50;
        state.set_fill_targets(targets);
        Some(arm_owned_fill_capture(&mut state, &dir).await)
    } else {
        crate::auto_zero::launch(&state);
        None
    };
    assert!(state.auto_zero.lock().unwrap().pending());
    if !kg50 {
        let mut targets = state.fill_targets_snapshot();
        targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
        state.set_fill_targets(targets);
    }
    let overflow = test_db_overflow();
    let timestamp = |i: u64| {
        if case == "cadence" {
            (i + 1) * 100000 / 3986
        } else {
            1 + i * 25
        }
    };
    let kind = if kg50 {
        "KG50"
    } else if case == "combined" {
        "DAQ_LOADCELL_CHAMBER"
    } else {
        "KG1000"
    };
    let values = if case == "combined" {
        vec![0.2, 300.0, 1.5]
    } else {
        vec![0.2]
    };
    let packet = |i: u64| {
        Packet::new(
            crate::telemetry_schema::data_type(kind),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            1767225700000 + timestamp(i),
            f32_payload(&values),
        )
        .unwrap()
    };
    let first_owner = packet(0);
    let saved_zero = || {
        let cfg = state.loadcell_calibration.lock().unwrap();
        if kg50 {
            cfg.extra_channels["kg50"].zero_raw
        } else {
            cfg.ch1_zero_raw
        }
    };
    let raw_kind = if kg50 { "KG50" } else { "KG1000" };
    let mut recorded_raw = 0;
    let mut recorded_sources = Vec::new();
    let expected_payload = payload_json_from_pkt(&first_owner);
    for i in 0..199 {
        std::fs::write(dir.join("clock"), (100000 + timestamp(i)).to_string()).unwrap();
        handle_packet(
            &state,
            &db_tx,
            &overflow,
            if i == 0 {
                first_owner.clone()
            } else {
                packet(i)
            },
        )
        .await;
        while let Ok(item) = db_rx.try_recv() {
            if let DbQueueItem::Write(DbWrite::Telemetry {
                data_type,
                sender_id,
                source_timestamp_ms,
                values_json,
                payload_json,
                ..
            }) = item
                && data_type == raw_kind
            {
                assert_eq!(sender_id, "DAQ");
                recorded_sources.push(source_timestamp_ms.unwrap());
                assert_eq!(payload_json, expected_payload);
                assert_eq!(
                    serde_json::from_str::<Vec<f32>>(&values_json.unwrap()).unwrap(),
                    vec![0.2]
                );
                recorded_raw += 1;
            }
        }
    }
    if case == "replay" {
        std::fs::write(dir.join("clock"), (100001 + timestamp(198)).to_string()).unwrap();
        // Deliberately invoke the handler directly with the SAME logical owner,
        // bypassing the router duplicate cache to test capture's own bound.
        handle_packet(&state, &db_tx, &overflow, first_owner.clone()).await;
        while let Ok(item) = db_rx.try_recv() {
            if let DbQueueItem::Write(DbWrite::Telemetry {
                data_type,
                sender_id,
                source_timestamp_ms,
                values_json,
                payload_json,
                ..
            }) = item
                && data_type == raw_kind
            {
                assert_eq!(sender_id, "DAQ");
                recorded_sources.push(source_timestamp_ms.unwrap());
                assert_eq!(payload_json, expected_payload);
                assert_eq!(
                    serde_json::from_str::<Vec<f32>>(&values_json.unwrap()).unwrap(),
                    vec![0.2]
                );
                recorded_raw += 1;
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        state.auto_zero.lock().unwrap().pending(),
        "199 distinct samples cannot satisfy the unchanged minimum"
    );
    assert!(saved_zero().is_none());
    if blocked {
        while db_tx
            .try_send(DbQueueItem::Control(RecordingCommand::Pause))
            .is_ok()
        {}
        assert_eq!(db_tx.capacity(), 0);
    }
    std::fs::write(dir.join("clock"), (100000 + timestamp(199)).to_string()).unwrap();
    let mut last = Box::pin(handle_packet(&state, &db_tx, &overflow, packet(199)));
    let late_start_fill_admitted = kg50 && state.is_command_allowed(&TelemetryCommand::StartFill);
    if blocked {
        // This child runs a single-thread executor. Poll the actual handler to
        // its FIRST recorder wait, then inspect live input without yielding to
        // the independently armed zero-completion task.
        let first = std::future::poll_fn(|cx| {
            std::task::Poll::Ready(std::future::Future::poll(last.as_mut(), cx))
        })
        .await;
        assert!(first.is_pending(), "full DB must stop the real handler");
        assert_eq!(db_tx.capacity(), 0);
        assert!(state.auto_zero.lock().unwrap().pending());
        assert!(
            saved_zero().is_none(),
            "tare completion has not run at this boundary"
        );
        {
            let rt = state.gse.lock().unwrap();
            let ingress = crate::gse::mass_ingress_snapshot(&rt);
            assert_eq!(
                ingress["source_timestamp_ms"].as_i64(),
                Some(1767225700000 + timestamp(199) as i64),
                "canonical calibrated mass must advance before raw/composite recorder backpressure"
            );
            if kg50 {
                eprintln!("KG50 BEFORE_RECORDER_WAIT: {ingress}");
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), last.as_mut())
                .await
                .is_err(),
            "actual handler must remain held at the full recording queue"
        );
    } else {
        last.as_mut().await;
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.auto_zero.lock().unwrap().pending() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("200th live sample must reach capture independently of a blocked recorder");
    assert_eq!(
        saved_zero(),
        Some(0.2),
        "valid stable source at declared cadence must keep its capture opportunity"
    );
    if late_start_fill_admitted {
        let rt = state.gse.lock().unwrap();
        let after_tare = crate::gse::mass_ingress_snapshot(&rt);
        eprintln!("KG50 AFTER_TARE_LATE_FILL: {after_tare}");
        assert!(
            after_tare["source_timestamp_ms"].is_null(),
            "a valid late fill path must invalidate pre-tare mass provenance; zero is not a fresh DAQ sample"
        );
    }
    if blocked {
        while db_rx.try_recv().is_ok() {}
        last.as_mut().await;
    }
    while let Ok(item) = db_rx.try_recv() {
        if let DbQueueItem::Write(DbWrite::Telemetry {
            data_type,
            sender_id,
            source_timestamp_ms,
            payload_json,
            values_json,
            ..
        }) = item
            && data_type == raw_kind
        {
            assert_eq!(sender_id, "DAQ");
            recorded_sources.push(source_timestamp_ms.unwrap());
            assert_eq!(payload_json, expected_payload);
            assert_eq!(
                serde_json::from_str::<Vec<f32>>(&values_json.unwrap()).unwrap(),
                vec![0.2]
            );
            recorded_raw += 1;
        }
    }
    assert_eq!(
        recorded_raw,
        if case == "replay" { 201 } else { 200 },
        "raw recording samples remain intact; capture dedup does not filter the recorder"
    );
    let mut expected_sources: Vec<i64> = (0..200)
        .map(|i| (1767225700000 + timestamp(i)) as i64)
        .collect();
    if case == "replay" {
        expected_sources.push((1767225700000 + timestamp(0)) as i64);
    }
    expected_sources.sort_unstable();
    recorded_sources.sort_unstable();
    assert_eq!(
        recorded_sources, expected_sources,
        "source capture timestamps must survive normalized rows unchanged"
    );
    println!(
        "CASE {case}:200 distinct samples,last_at={}ms,stable band0kg,rawrecorded200,capture saved",
        timestamp(199)
    );
}

async fn prewindow_case() {
    let dir = std::path::PathBuf::from(std::env::var_os("GS_ZERO_INGRESS_ROOT").unwrap());
    let (db_tx, mut db_rx) = mpsc::channel(16);
    let (state, mut command_rx) = test_app_state_with_cmd_rx(db_tx.clone()).await;
    let mut cfg = loadcell::LoadcellCalibrationFile::default();
    cfg.ch1.m = Some(1.0);
    cfg.ch1.b = Some(0.0);
    cfg.ch1_zero_raw = Some(0.125);
    *state.loadcell_calibration.lock().unwrap() = cfg;
    *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    let overflow = test_db_overflow();
    let packet = |source| {
        Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            source,
            f32_payload(&[0.2]),
        )
        .unwrap()
    };
    while db_tx
        .try_send(DbQueueItem::Control(RecordingCommand::Pause))
        .is_ok()
    {}
    let mut old = Box::pin(handle_packet(
        &state,
        &db_tx,
        &overflow,
        packet(1767225700000),
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), old.as_mut())
            .await
            .is_err()
    );
    std::fs::write(dir.join("clock"), "100010").unwrap();
    crate::auto_zero::launch(&state);
    while db_rx.try_recv().is_ok() {}
    old.as_mut().await;
    while db_rx.try_recv().is_ok() {}
    for i in 1..=199 {
        std::fs::write(dir.join("clock"), (100010 + i * 25).to_string()).unwrap();
        handle_packet(&state, &db_tx, &overflow, packet(1767225700010 + i * 25)).await;
        while db_rx.try_recv().is_ok() {}
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        state.auto_zero.lock().unwrap().pending(),
        "prewindow input must not be retimestamped by disk delay"
    );
    assert_eq!(
        state.loadcell_calibration.lock().unwrap().ch1_zero_raw,
        Some(0.125)
    );
    std::fs::write(dir.join("clock"), "107011").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.auto_zero.lock().unwrap().pending() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state.loadcell_calibration.lock().unwrap().ch1_zero_raw,
        Some(0.125),
        "insufficient samples retain the previous valid tare"
    );
    assert!(
        command_rx.try_recv().is_err(),
        "capture failure must never enqueue Abort or re-arm a command"
    );
}

#[test]
fn incident_recorded_kg50_is_stable_but_loaded_and_must_not_erase_previous_tare() {
    isolated_zero_case(
        "recorded_load",
        "telemetry_task::tests::zero_ingress::incident_recorded_kg50_is_stable_but_loaded_and_must_not_erase_previous_tare",
    );
}
async fn recorded_load_case(launch: bool) {
    let fixture: serde_json::Value = serde_json::from_str(if launch {
        include_str!("../auto_zero/fixtures/incident-2026-10-31-kg1000.json")
    } else {
        include_str!("../auto_zero/fixtures/incident-2026-10-31-kg50.json")
    })
    .unwrap();
    let calibration: loadcell::LoadcellCalibrationFile =
        serde_json::from_value(fixture["calibration"].clone()).unwrap();
    let (db_tx, mut db_rx) = mpsc::channel(16);
    let mut state = if launch {
        test_app_state(db_tx.clone()).await
    } else {
        crate::gse::fill_recording_test_state(0., gse_sequence::Phase::Passed, None)
            .await
            .0
    };
    *state.state.lock().unwrap() = FlightState::Idle;
    *state.loadcell_calibration.lock().unwrap() = calibration.clone();
    let targets: crate::fill_targets::FillTargetsConfig =
        serde_json::from_value(fixture["fill_targets"].clone()).unwrap();
    state.set_fill_targets(targets);
    *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    assert_eq!(
        state.fill_targets_snapshot().fill_source,
        crate::fill_targets::FillSource::Kg50
    );
    let dir = std::path::PathBuf::from(std::env::var_os("GS_ZERO_INGRESS_ROOT").unwrap());
    let _fill_owner = if launch {
        crate::auto_zero::launch(&state);
        None
    } else {
        Some(arm_owned_fill_capture(&mut state, &dir).await)
    };
    let samples = fixture["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 200);
    let start = samples[0]["timestamp_ms"].as_u64().unwrap();
    let mut weights = Vec::new();
    let overflow = test_db_overflow();
    for sample in samples {
        let elapsed = sample["timestamp_ms"].as_u64().unwrap() - start;
        std::fs::write(dir.join("clock"), (100000 + elapsed).to_string()).unwrap();
        let raw = sample["raw"].as_f64().unwrap() as f32;
        weights.push(
            loadcell::calibrated_weight_kg(
                &calibration,
                if launch { "KG1000" } else { "KG50" },
                raw,
            )
            .unwrap(),
        );
        let packet = Packet::new(
            crate::telemetry_schema::data_type(if launch {
                "DAQ_LOADCELL_CHAMBER"
            } else {
                "DAQ_KG50_SELECTED"
            }),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "DAQ",
            sample["source_timestamp_ms"].as_u64().unwrap(),
            serde_json::from_value::<Vec<u8>>(sample["payload"].clone())
                .unwrap()
                .into(),
        )
        .unwrap();
        handle_packet(&state, &db_tx, &overflow, packet).await;
        while db_rx.try_recv().is_ok() {}
    }
    weights.sort_by(f32::total_cmp);
    let trimmed = &weights[10..190];
    let mean = trimmed.iter().map(|v| *v as f64).sum::<f64>() / 180.0;
    eprintln!(
        "Actual recorded {}: trimmed calibrated mean={mean:.6}kg, range={:.6}kg, span={}ms",
        if launch { "KG1000" } else { "KG50" },
        trimmed[179] - trimmed[0],
        samples[199]["timestamp_ms"].as_u64().unwrap() - start
    );
    if launch {
        assert!(mean.abs() <= 0.8);
        assert!(trimmed[179] - trimmed[0] <= 0.8);
        assert_eq!(samples[199]["timestamp_ms"].as_u64().unwrap() - start, 5010);
    } else {
        assert!((mean - 4.089860).abs() < 0.001);
        assert!(trimmed[179] - trimmed[0] < 0.05);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.auto_zero.lock().unwrap().pending() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let current = state.loadcell_calibration.lock().unwrap().clone();
    let notifications = state.notifications_snapshot();
    if launch {
        assert!(
            notifications
                .iter()
                .any(|n| n.message.contains("KG1000 automatic zero saved")),
            "the actual stable200-point5010ms window must fit the bounded7s budget; notifications={}",
            serde_json::to_string(&notifications).unwrap()
        );
        let zero = current.ch1_zero_raw.expect("validzeroapplied");
        assert!(
            loadcell::calibrated_weight_kg(&current, "KG1000", zero)
                .unwrap()
                .abs()
                < 0.0001
        );
        assert_eq!(
            current.extra_channels["kg50"].zero_raw,
            calibration.extra_channels["kg50"].zero_raw
        );
    } else {
        assert_eq!(
            serde_json::to_value(&current).unwrap(),
            serde_json::to_value(&calibration).unwrap()
        );
        assert!(
            notifications
                .iter()
                .any(|n| n.message.contains("Load exceeds the ±0.8 kg")),
            "loaded/shifted baseline must be reported, not hidden as timeout or abort"
        );
        assert!(
            !notifications
                .iter()
                .any(|n| n.message.contains("automatic zero saved"))
        );
    }
    assert_ne!(*state.state.lock().unwrap(), FlightState::Aborted);
}

#[test]
fn foreign_source_never_seeds_automatic_tare_or_mass_but_raw_rows_remain() {
    isolated_zero_case(
        "foreign",
        "telemetry_task::tests::zero_ingress::foreign_source_never_seeds_automatic_tare_or_mass_but_raw_rows_remain",
    );
}
#[test]
fn stale_source_never_seeds_automatic_tare_or_mass_but_raw_rows_remain() {
    isolated_zero_case(
        "stale_source",
        "telemetry_task::tests::zero_ingress::stale_source_never_seeds_automatic_tare_or_mass_but_raw_rows_remain",
    );
}

async fn rejected_source_case(case: &str) {
    let (db_tx, mut db_rx) = mpsc::channel(16);
    let state = test_app_state(db_tx.clone()).await;
    let mut calibration = loadcell::LoadcellCalibrationFile::default();
    calibration.ch1.m = Some(1.0);
    calibration.ch1.b = Some(0.0);
    calibration.ch1_zero_raw = None;
    *state.loadcell_calibration.lock().unwrap() = calibration;
    let mut targets = state.fill_targets_snapshot();
    targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
    state.set_fill_targets(targets);
    *state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    crate::auto_zero::launch(&state);
    let dir = std::path::PathBuf::from(std::env::var_os("GS_ZERO_INGRESS_ROOT").unwrap());
    let overflow = test_db_overflow();
    let mut raw_rows = 0;
    for i in 0..200u64 {
        std::fs::write(dir.join("clock"), (100000 + i * 25).to_string()).unwrap();
        let source = 1767225700000 + i * 25 - if case == "stale_source" { 5000 } else { 0 };
        let packet = Packet::new(
            crate::telemetry_schema::data_type("KG1000"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            if case == "foreign" { "GB" } else { "DAQ" },
            source,
            f32_payload(&[0.2]),
        )
        .unwrap();
        handle_packet(&state, &db_tx, &overflow, packet).await;
        while let Ok(item) = db_rx.try_recv() {
            if let DbQueueItem::Write(DbWrite::Telemetry { data_type, .. }) = item
                && data_type == "KG1000"
            {
                raw_rows += 1;
            }
        }
    }
    assert_eq!(
        raw_rows, 200,
        "source rejection cannot erase original recorded samples"
    );
    let mut capture = state.auto_zero.lock().unwrap();
    assert_eq!(capture.captured_source_counts_for_test(), [0, 0]);
    capture.cancel();
    drop(capture);
    let rt = state.gse.lock().unwrap();
    assert!(crate::gse::mass_ingress_snapshot(&rt)["source_timestamp_ms"].is_null());
    assert!(
        state
            .loadcell_calibration
            .lock()
            .unwrap()
            .ch1_zero_raw
            .is_none()
    );
}

#[test]
fn incident_recorded_kg1000_composite_needs_margin_but_preserves_stability_quality() {
    isolated_zero_case(
        "recorded_launch",
        "telemetry_task::tests::zero_ingress::incident_recorded_kg1000_composite_needs_margin_but_preserves_stability_quality",
    );
}
