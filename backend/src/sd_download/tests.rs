use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use sedsnet::router::{P2pStreamEvent, RouterConfig};
use std::sync::atomic::AtomicUsize;

#[test]
fn only_idle_landed_and_recovery_admit_downloads() {
    use FlightState::*;
    for state in [
        Startup,
        Idle,
        PreFill,
        FillTest,
        NitrogenFill,
        NitrousFill,
        Armed,
        Launch,
        Ascent,
        Coast,
        Apogee,
        Descent,
        Reefing,
        Landed,
        Recovery,
        Aborted,
    ] {
        assert_eq!(
            download_allowed(state),
            matches!(state, Idle | Landed | Recovery),
            "{state:?}"
        );
    }
}

#[test]
fn wire_codec_matches_firmware_layout() {
    assert_eq!(encode_list(2, 0x0105), [0x21, 2, 5, 1]);
    let read = encode_read(0x0102_0304, 0, "A.CSV");
    assert_eq!(&read[..7], &[0x22, 4, 3, 2, 1, 0, 5]);
    assert_eq!(&read[7..], b"A.CSV");
    let mut data = vec![0xa2, 0, 1, 0, 2, 0, 0, 0x10, 0, 0, 0];
    data.extend_from_slice(b"xyz");
    assert_eq!(
        decode_frame(&data),
        Some(Frame::Data(DataFrame {
            status: 0,
            flags: 1,
            offset: 512,
            file_size: 16,
            data: b"xyz".to_vec()
        }))
    );
    let mut list = vec![0xa1, 0, 1, 3, 0, 42, 0, 0, 0, 1, 5, 0];
    list.extend_from_slice(b"L.CSV");
    assert_eq!(
        decode_frame(&list),
        Some(Frame::List(ListFrame {
            status: 0,
            dir: 1,
            index: 3,
            size: 42,
            flags: 1,
            name: "L.CSV".into()
        }))
    );
    list.push(0);
    assert_eq!(
        decode_frame(&list),
        None,
        "name length must match the frame"
    );
    for name in ["..", "a/b", "a\\b", "", ".", "x y", &"A".repeat(64)] {
        assert!(!valid_name(name), "{name:?}");
    }
    assert!(valid_name("DAQ_LAUNCH_1700000000000_000.CSV"));
}

// ------------------------------------------------------------ mock board

struct MockBoard {
    files: Vec<(u8, String, Vec<u8>)>,
    reads: AtomicUsize,
    /// After this many READs, run `on_read` (cancel or state flip).
    trigger_after: usize,
    on_read: Box<dyn Fn() + Send + Sync>,
}

fn link() -> (Arc<Router>, Arc<Router>) {
    let gs = Arc::new(Router::new(
        RouterConfig::default()
            .with_hostname("GS")
            .with_static_address(0x2001),
    ));
    let daq = Arc::new(Router::new(
        RouterConfig::default()
            .with_hostname("DAQ")
            .with_static_address(0x2002),
    ));
    let daq_rx = daq.clone();
    gs.add_side_packet("to-daq", move |packet| daq_rx.rx_from_side(packet, 0));
    let gs_rx = gs.clone();
    daq.add_side_packet("to-gs", move |packet| gs_rx.rx_from_side(packet, 0));
    gs.announce_discovery().unwrap();
    daq.announce_discovery().unwrap();
    for _ in 0..3 {
        gs.process_all_queues().unwrap();
        daq.process_all_queues().unwrap();
    }
    assert!(gs.resolve_hostname("DAQ").is_some());
    (gs, daq)
}

fn serve(daq: &Arc<Router>, board: Arc<MockBoard>) {
    let reply_router = daq.clone();
    daq.bind_p2p_stream_port(SD_FILE_STREAM_PORT, move |event: P2pStreamEvent<'_>| {
        if event.kind != P2pStreamEventKind::Data || event.payload.is_empty() {
            return Ok(());
        }
        let p = event.payload;
        let mut frames: Vec<Vec<u8>> = Vec::new();
        match p[0] {
            OP_LIST => {
                let (dir, index) = (p[1], u16::from_le_bytes([p[2], p[3]]) as usize);
                let entry = board.files.iter().filter(|f| f.0 == dir).nth(index);
                let mut f = vec![
                    0xa1,
                    if entry.is_some() { 0 } else { STATUS_END },
                    dir,
                    p[2],
                    p[3],
                ];
                let (size, name) = entry.map_or((0, ""), |e| (e.2.len() as u32, e.1.as_str()));
                f.extend_from_slice(&size.to_le_bytes());
                f.extend_from_slice(&[0, name.len() as u8, 0]);
                f.extend_from_slice(name.as_bytes());
                frames.push(f);
            }
            OP_READ => {
                let n = board.reads.fetch_add(1, Ordering::SeqCst) + 1;
                if n == board.trigger_after {
                    (board.on_read)();
                }
                let offset = u32::from_le_bytes(p[1..5].try_into().unwrap()) as usize;
                let name = std::str::from_utf8(&p[7..]).unwrap();
                let data = &board
                    .files
                    .iter()
                    .find(|f| f.0 == p[5] && f.1 == name)
                    .unwrap()
                    .2;
                let header = |status: u8, flags: u8, off: usize| {
                    let mut f = vec![0xa2, status, flags];
                    f.extend_from_slice(&(off as u32).to_le_bytes());
                    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    f
                };
                if offset >= data.len() {
                    frames.push(header(STATUS_END, FLAG_LAST, offset));
                } else {
                    let end = ((offset / 512 + 1) * 512).min(data.len());
                    let mut at = offset;
                    while at < end {
                        let to = (at + 100).min(end);
                        let mut f = header(0, if to == end { FLAG_LAST } else { 0 }, at);
                        f.extend_from_slice(&data[at..to]);
                        frames.push(f);
                        at = to;
                    }
                }
            }
            OP_CANCEL => frames.push(vec![0xa3, 0]),
            _ => {}
        }
        for f in frames {
            reply_router.send_p2p_stream(event.stream_id, &f)?;
        }
        Ok(())
    })
    .unwrap();
}

fn pump(gs: Arc<Router>, daq: Arc<Router>) -> (Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let task = tokio::spawn(async move {
        while flag.load(Ordering::Relaxed) {
            gs.process_all_queues().unwrap();
            daq.process_all_queues().unwrap();
            tokio::task::yield_now().await;
        }
    });
    (running, task)
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn open_gate() -> Gate {
    Arc::new(|| None)
}

fn request(id: u64, root: &FsPath, name: &str) -> TransferRequest {
    TransferRequest {
        id,
        board: Board::DaqBoard,
        dir: 0,
        name: name.into(),
        root: root.into(),
        max_bps: 10_000_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_downloads_resumes_and_verifies_partial_tail() {
    let (gs, daq) = link();
    let launch = pattern(3 * 512 + 77, 1);
    let utc = pattern(900, 9);
    let board = Arc::new(MockBoard {
        files: vec![
            (0, "DAQ_LAUNCH_1_000.CSV".into(), launch.clone()),
            (1, "DAQ_1_000.CSV".into(), utc),
        ],
        reads: AtomicUsize::new(0),
        trigger_after: usize::MAX,
        on_read: Box::new(|| {}),
    });
    serve(&daq, board.clone());
    let (running, task) = pump(gs.clone(), daq.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));

    let listing = refresh_listing(manager, gs.clone(), Board::DaqBoard, open_gate())
        .await
        .unwrap();
    assert_eq!(
        listing
            .files
            .iter()
            .map(|f| (f.dir.as_str(), f.name.as_str(), f.size))
            .collect::<Vec<_>>(),
        vec![
            ("LAUNCH", "DAQ_LAUNCH_1_000.CSV", launch.len() as u64),
            ("UTC", "DAQ_1_000.CSV", 900)
        ]
    );

    let root = TestDirectory::new();
    let (final_path, part_path) =
        local_paths(&root.0, Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_1_000.CSV");
    // Resume: a 700-byte partial continues from byte 700.
    std::fs::create_dir_all(part_path.parent().unwrap()).unwrap();
    std::fs::write(&part_path, &launch[..700]).unwrap();
    let (id, cancel) = manager
        .create_job(Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_1_000.CSV")
        .unwrap();
    transfer(
        manager,
        gs.clone(),
        &request(id, &root.0, "DAQ_LAUNCH_1_000.CSV"),
        &open_gate(),
        &cancel,
    )
    .await
    .unwrap();
    let status = manager.get(id).unwrap();
    assert_eq!(
        (status.phase, status.resumed_from, status.bytes_done),
        (Phase::Complete, 700, launch.len() as u64)
    );
    assert_eq!(std::fs::read(&final_path).unwrap(), launch);
    assert!(!part_path.exists());
    assert!(completed_file(&root.0, "DAQ", "LAUNCH", "DAQ_LAUNCH_1_000.CSV").is_some());
    assert!(completed_file(&root.0, "DAQ", "LAUNCH", "../x").is_none());

    // Starting a finished file again only re-checks its tail and completes.
    let (id, cancel) = manager.create_job(Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_1_000.CSV").unwrap();
    transfer(manager, gs.clone(), &request(id, &root.0, "DAQ_LAUNCH_1_000.CSV"), &open_gate(), &cancel)
        .await
        .unwrap();
    assert_eq!(manager.get(id).unwrap().resumed_from, launch.len() as u64);
    assert_eq!(std::fs::read(&final_path).unwrap(), launch);

    // A partial whose tail no longer matches the card restarts from zero.
    std::fs::remove_file(&final_path).unwrap();
    let mut stale = launch[..1000].to_vec();
    stale[999] ^= 0xff;
    std::fs::write(&part_path, &stale).unwrap();
    let (id, cancel) = manager
        .create_job(Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_1_000.CSV")
        .unwrap();
    transfer(
        manager,
        gs.clone(),
        &request(id, &root.0, "DAQ_LAUNCH_1_000.CSV"),
        &open_gate(),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(manager.get(id).unwrap().resumed_from, 0);
    assert_eq!(std::fs::read(&final_path).unwrap(), launch);

    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_keeps_partial_and_a_later_start_resumes_it() {
    let (gs, daq) = link();
    let data = pattern(8 * 512, 3);
    let cancel = Arc::new(AtomicBool::new(false));
    let trip = cancel.clone();
    let board = Arc::new(MockBoard {
        files: vec![(0, "F.CSV".into(), data.clone())],
        reads: AtomicUsize::new(0),
        trigger_after: 3,
        on_read: Box::new(move || trip.store(true, Ordering::SeqCst)),
    });
    serve(&daq, board.clone());
    let (running, task) = pump(gs.clone(), daq.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let root = TestDirectory::new();
    let (id, _) = manager
        .create_job(Board::DaqBoard, "LAUNCH", "F.CSV")
        .unwrap();
    let error = transfer(
        manager,
        gs.clone(),
        &request(id, &root.0, "F.CSV"),
        &open_gate(),
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, StreamError::Cancelled), "{error:?}");
    let (final_path, part_path) = local_paths(&root.0, Board::DaqBoard, "LAUNCH", "F.CSV");
    let kept = std::fs::metadata(&part_path).unwrap().len();
    assert!(
        kept > 0 && kept < data.len() as u64 && kept % 512 == 0,
        "kept {kept}"
    );
    assert!(!final_path.exists());
    // Only one job may be active; a finished one does not block the next.
    manager.update(id, |s| s.phase = Phase::Cancelled);
    let (id2, cancel2) = manager
        .create_job(Board::DaqBoard, "LAUNCH", "F.CSV")
        .unwrap();
    assert!(
        manager
            .create_job(Board::DaqBoard, "LAUNCH", "F.CSV")
            .unwrap_err()
            .contains("already active")
    );
    assert!(
        manager.cancel(id).is_err(),
        "inactive jobs cannot be cancelled again"
    );
    transfer(
        manager,
        gs.clone(),
        &request(id2, &root.0, "F.CSV"),
        &open_gate(),
        &cancel2,
    )
    .await
    .unwrap();
    assert_eq!(manager.get(id2).unwrap().resumed_from, kept);
    assert_eq!(std::fs::read(&final_path).unwrap(), data);
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_idle_pauses_the_transfer_and_refuses_new_work() {
    let (gs, daq) = link();
    let data = pattern(6 * 512, 5);
    let blocked = Arc::new(AtomicBool::new(false));
    let trip = blocked.clone();
    let board = Arc::new(MockBoard {
        files: vec![(0, "F.CSV".into(), data.clone())],
        reads: AtomicUsize::new(0),
        trigger_after: 2,
        on_read: Box::new(move || trip.store(true, Ordering::SeqCst)),
    });
    serve(&daq, board);
    let (running, task) = pump(gs.clone(), daq.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let flag = blocked.clone();
    let gate: Gate = Arc::new(move || flag.load(Ordering::SeqCst).then(|| "now Armed".to_string()));
    let root = TestDirectory::new();
    let (id, cancel) = manager
        .create_job(Board::DaqBoard, "LAUNCH", "F.CSV")
        .unwrap();
    let error = transfer(
        manager,
        gs.clone(),
        &request(id, &root.0, "F.CSV"),
        &gate,
        &cancel,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, StreamError::Gate(ref r) if r == "now Armed"),
        "{error:?}"
    );
    let (_, part_path) = local_paths(&root.0, Board::DaqBoard, "LAUNCH", "F.CSV");
    assert!(std::fs::metadata(&part_path).unwrap().len() < data.len() as u64);
    // Neither a new listing nor a new start is admitted while blocked.
    manager.update(id, |s| s.phase = Phase::Paused);
    assert_eq!(
        refresh_listing(manager, gs.clone(), Board::DaqBoard, gate.clone())
            .await
            .unwrap_err(),
        "now Armed"
    );
    assert_eq!(
        start_job(
            manager,
            gs.clone(),
            Board::DaqBoard,
            "LAUNCH",
            "F.CSV",
            root.0.clone(),
            gate.clone()
        )
        .unwrap_err(),
        "now Armed"
    );
    assert!(
        start_job(
            manager,
            gs.clone(),
            Board::FlightComputer,
            "LAUNCH",
            "F.CSV",
            root.0.clone(),
            open_gate()
        )
        .is_err()
    );
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

// ------------------------------------------------------------- HTTP gating

async fn auth_state(root: &FsPath) -> Arc<AppState> {
    let state = export_state(root, true).await;
    std::fs::write(
        root.join("auth.json"),
        serde_json::json!({"anonymous":{"view_data":true},"users":[
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}
        ]})
        .to_string(),
    )
    .unwrap();
    for (token, user, commands) in [("view-token", "viewer", 0), ("cmd-token", "operator", 1)] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms)VALUES(?,?,'session',1,?,0,9999999999999)")
            .bind(token).bind(user).bind(commands)
            .execute(&state.auth_db).await.unwrap();
    }
    state
}

async fn call(
    state: &Arc<AppState>,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: &str,
) -> StatusCode {
    use tower::ServiceExt;
    let app = routes().with_state(state.clone());
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(
        request
            .body(axum::body::Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn only_command_users_in_allowed_states_may_list_or_start() {
    let root = TestDirectory::new();
    let state = auth_state(&root.0).await;
    let start = r#"{"dir":"LAUNCH","name":"F.CSV"}"#;
    for (method, uri, body) in [
        ("POST", "/api/sd_download/daq/list", ""),
        ("POST", "/api/sd_download/daq/start", start),
        ("POST", "/api/sd_download/jobs/1/cancel", ""),
    ] {
        let anonymous = call(&state, method, uri, None, body).await;
        assert!(
            matches!(anonymous, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
            "{uri} {anonymous}"
        );
        assert_eq!(
            call(&state, method, uri, Some("view-token"), body).await,
            StatusCode::FORBIDDEN,
            "{uri}"
        );
    }
    // Viewers may still see the panel state.
    assert_eq!(
        call(&state, "GET", "/api/sd_download", Some("view-token"), "").await,
        StatusCode::OK
    );
    // Command users are refused outside Idle/Landed/Recovery (Startup here)...
    for flight in [
        FlightState::Startup,
        FlightState::NitrousFill,
        FlightState::Armed,
        FlightState::Launch,
        FlightState::Ascent,
    ] {
        *state.state.lock().unwrap() = flight;
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/sd_download/daq/start",
                Some("cmd-token"),
                start
            )
            .await,
            StatusCode::CONFLICT,
            "{flight:?}"
        );
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/sd_download/daq/list",
                Some("cmd-token"),
                ""
            )
            .await,
            StatusCode::CONFLICT,
            "{flight:?}"
        );
    }
    // ...and pass the gate in Idle/Landed/Recovery (the test state has no router).
    for flight in [
        FlightState::Idle,
        FlightState::Landed,
        FlightState::Recovery,
    ] {
        *state.state.lock().unwrap() = flight;
        assert_eq!(
            call(
                &state,
                "POST",
                "/api/sd_download/daq/start",
                Some("cmd-token"),
                start
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE,
            "{flight:?}"
        );
    }
    // Unsupported boards are rejected before any network action.
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/sd_download/vb/list",
            Some("cmd-token"),
            ""
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/api/sd_download/jobs/999/cancel",
            Some("cmd-token"),
            ""
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn completed_download_feeds_the_unchanged_sd_import() {
    use tower::ServiceExt;
    let root = TestDirectory::new();
    let state = export_state(&root.0, true).await;
    std::fs::write(root.0.join("auth.json"),serde_json::json!({"anonymous":{"view_data":true},"users":[{"username":"sd-admin","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}]}).to_string()).unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms)VALUES('sd-token','sd-admin','session',1,0,0,9999999999999)").execute(&state.auth_db).await.unwrap();
    let source = "groundstation_recording_123.db";
    let (db, _) = crate::telemetry_db::open_telemetry_db(&root.0.join(source)).await.unwrap();
    sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json) VALUES(1700000005000,1700000000000,'LOADCELL_50_WEIGHT_KG','DAQ','[9]','[1,2]')").execute(&db).await.unwrap();
    db.close().await;
    let before = std::fs::read(root.0.join(source)).unwrap();
    let csv = include_str!("../test-fixtures/sd-recovery-current-firmware.csv");
    let folder = download_root(&state).join("DAQ").join("LAUNCH");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("DAQ_LAUNCH_1_000.CSV"), csv).unwrap();
    std::fs::write(folder.join("PART_000.CSV.part"), csv).unwrap();
    let app = crate::recording_export::routes().merge(routes()).with_state(state.clone());
    let post = |uri: String, body: serde_json::Value| {
        axum::http::Request::post(uri)
            .header("content-type", "application/json")
            .header("authorization", "Bearer sd-token")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    };
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../test-fixtures/sd-recovery-manifest.json")).unwrap();
    let init = app.clone().oneshot(post(format!("/api/recordings/{source}/sd/imports"), serde_json::json!({"filename":"DAQ_LAUNCH_1_000.CSV","manifest":manifest}))).await.unwrap();
    assert_eq!(init.status(), StatusCode::OK);
    let init: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(init.into_body(), 1 << 20).await.unwrap()).unwrap();
    let token = init["upload_id"].as_str().unwrap();
    // Partial or unknown station copies are refused.
    for name in ["PART_000.CSV", "../auth.json", "MISSING.CSV"] {
        let refused = app.clone().oneshot(post(format!("/api/recordings/{source}/sd/imports/{token}/download"), serde_json::json!({"board":"DAQ","dir":"LAUNCH","name":name}))).await.unwrap();
        assert_eq!(refused.status(), StatusCode::NOT_FOUND, "{name}");
    }
    let imported = app.clone().oneshot(post(format!("/api/recordings/{source}/sd/imports/{token}/download"), serde_json::json!({"board":"DAQ","dir":"LAUNCH","name":"DAQ_LAUNCH_1_000.CSV"}))).await.unwrap();
    let status = imported.status();
    let body = axum::body::to_bytes(imported.into_body(), 1 << 20).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let imported: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(imported["recording"].as_str().unwrap().starts_with("groundstation_recording_"));
    assert_eq!(imported["summary"]["network_suppressed_in_resolved_view"], 1, "SD rows win over network rows");
    assert_eq!(std::fs::read(root.0.join(source)).unwrap(), before, "original recording unchanged");
}
