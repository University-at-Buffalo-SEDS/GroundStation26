use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use sedsnet::router::RouterConfig;
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
fn wire_codec_matches_the_protocol_layout() {
    assert_eq!(encode_list(2, 0x0105), [0x21, 2, 5, 1]);
    let open = xfer::Open { xfer: 9, dir: 1, flags: xfer::OPEN_SDX, offset: 0x0102_0304, length: 0, rate: 48, msg: 460, window: 6, block_kib: 16, name: "A.CSV".into() };
    let bytes = open.encode();
    assert_eq!(&bytes[..19], &[0x25, 9, 1, 1, 4, 3, 2, 1, 0, 0, 0, 0, 48, 0, 0xcc, 1, 6, 16, 5]);
    assert_eq!(xfer::Open::decode(&bytes), Some(open));
    let answer = xfer::OpenAnswer { status: 0, xfer: 9, flags: 5, size: 1000, offset: 10, length: 990, msg: 460, block_kib: 16 };
    let bytes = answer.encode();
    assert_eq!(bytes.len(), 19);
    assert_eq!((bytes[0], u32::from_le_bytes(bytes[4..8].try_into().unwrap()), bytes[18]), (0xa5, 1000, 16));
    assert_eq!(decode_frame(&bytes), Some(Frame::Open(answer)));
    let ack = xfer::Ack { xfer: 9, una: 0x0201, bitmap: 0x8000_0001, rate: 96, window: 6 };
    assert_eq!(ack.encode(), vec![0x26, 9, 1, 2, 1, 0, 0, 0x80, 96, 0, 6]);
    let body = b"xyz";
    let data = xfer::Data { xfer: 9, flags: xfer::DATA_FIRST | xfer::DATA_LAST | xfer::DATA_SDX, seq: 7, block: 512, raw_len: 3, crc: 0xdead_beef, body };
    let bytes = data.encode();
    assert_eq!(bytes.len(), 17 + 3);
    assert_eq!(&bytes[..9], &[0xa6, 9, 7, 7, 0, 0, 2, 0, 0]);
    assert_eq!(xfer::Data::decode(&bytes), Some(data));
    let mid = xfer::Data { flags: 0, raw_len: 0, crc: 0, ..xfer::Data::decode(&bytes).unwrap() }.encode();
    assert_eq!(mid.len(), 9 + 3, "no trailer without LAST");
    assert_eq!(xfer::encode_hash(0, "A.CSV"), [&[0x27, 0, 5][..], b"A.CSV"].concat());
    let hash = xfer::HashAnswer { status: 0, flags: 4, size: 77, sha256: [3; 32] };
    assert_eq!(hash.encode().len(), 39);
    assert_eq!(decode_frame(&hash.encode()), Some(Frame::Hash(hash)));
    let mut list = vec![0xa1, 0, 1, 3, 0, 42, 0, 0, 0, 1, 5, 0];
    list.extend_from_slice(b"L.CSV");
    assert_eq!(
        decode_frame(&list),
        Some(Frame::List(ListFrame { status: 0, dir: 1, index: 3, size: 42, flags: 1, name: "L.CSV".into() }))
    );
    list.push(0);
    assert_eq!(decode_frame(&list), None, "name length must match the frame");
    for name in ["..", "a/b", "a\\b", "", ".", "x y", &"A".repeat(64)] {
        assert!(!valid_name(name), "{name:?}");
    }
    assert!(valid_name("DAQ_LAUNCH_1700000000000_000.CSV"));
}

// ------------------------------------------------------------ mock board

/// A board's SD card served with protocol v2 by the shared engine
/// (`xfer::serve`), as the FC and DAQ firmware serve it.
pub(super) struct MockCard {
    /// (dir, name, bytes, still open)
    pub files: Mutex<Vec<(u8, String, Vec<u8>, bool)>>,
    pub opens: AtomicUsize,
    /// Raw bytes cut into blocks (what the board read for the GS).
    pub served: AtomicU64,
    /// Corrupt the CRC-32 of this many blocks.
    pub corrupt: AtomicUsize,
    /// Run `on_trigger` once `served` reaches this.
    pub trigger_at: AtomicU64,
    pub on_trigger: Mutex<Box<dyn FnMut() + Send>>,
    /// When ACKs with rate 0 (pauses) arrived, and the last ACK's rate.
    pub pause_acks: Mutex<Vec<Instant>>,
    last_rate: AtomicU64,
    /// A board that needs this long to hash a file: BUSY until then; a CANCEL
    /// abandons the hash and the next HASH starts it over (DAQ-Board).
    pub hash_takes: Mutex<Option<Duration>>,
    hash_started: Mutex<Option<Instant>>,
    pub hash_restarts: AtomicUsize,
}

impl MockCard {
    pub(super) fn new(files: Vec<(u8, &str, Vec<u8>)>) -> Self {
        Self {
            files: Mutex::new(files.into_iter().map(|(d, n, b)| (d, n.to_string(), b, false)).collect()),
            opens: AtomicUsize::new(0),
            served: AtomicU64::new(0),
            corrupt: AtomicUsize::new(0),
            trigger_at: AtomicU64::new(u64::MAX),
            on_trigger: Mutex::new(Box::new(|| {})),
            pause_acks: Mutex::new(Vec::new()),
            last_rate: AtomicU64::new(1),
            hash_takes: Mutex::new(None),
            hash_started: Mutex::new(None),
            hash_restarts: AtomicUsize::new(0),
        }
    }
    pub(super) fn trigger(&self, at: u64, f: impl FnMut() + Send + 'static) {
        *self.on_trigger.lock().unwrap() = Box::new(f);
        self.trigger_at.store(at, Ordering::SeqCst);
    }
}

impl xfer::Catalog for MockCard {
    fn list(&self, dir: u8, index: u16) -> Option<(String, u64, bool)> {
        let files = self.files.lock().unwrap();
        files.iter().filter(|f| f.0 == dir).nth(usize::from(index)).map(|f| (f.1.clone(), f.2.len() as u64, f.3))
    }
    fn file(&self, dir: u8, name: &str) -> Result<xfer::Snapshot, u8> {
        let files = self.files.lock().unwrap();
        let f = files.iter().find(|f| f.0 == dir && f.1 == name).ok_or(xfer::STATUS_NOT_FOUND)?;
        Ok(xfer::Snapshot { bytes: Arc::new(f.2.clone()), growing: f.3 })
    }
    fn hash(&self, dir: u8, name: &str) -> Result<(u64, [u8; 32], bool), u8> {
        if let Some(takes) = *self.hash_takes.lock().unwrap() {
            let started = *self.hash_started.lock().unwrap().get_or_insert_with(Instant::now);
            if started.elapsed() < takes {
                return Err(xfer::STATUS_BUSY);
            }
            *self.hash_started.lock().unwrap() = None;
        }
        let snap = self.file(dir, name)?;
        Ok((snap.bytes.len() as u64, xfer::sha256(&snap.bytes), snap.growing))
    }
    fn on_cancel(&self) {
        if self.hash_started.lock().unwrap().take().is_some() {
            self.hash_restarts.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn on_open(&self, _open: &xfer::Open) {
        self.opens.fetch_add(1, Ordering::SeqCst);
    }
    fn corrupt_next_block(&self) -> bool {
        self.corrupt.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
    }
    fn on_ack(&self, ack: &xfer::Ack) {
        if self.last_rate.swap(u64::from(ack.rate), Ordering::SeqCst) != 0 && ack.rate == 0 {
            self.pause_acks.lock().unwrap().push(Instant::now());
        }
    }
    fn on_served(&self, raw: usize) {
        let total = self.served.fetch_add(raw as u64, Ordering::SeqCst) + raw as u64;
        if total >= self.trigger_at.load(Ordering::SeqCst) {
            self.trigger_at.store(u64::MAX, Ordering::SeqCst);
            (self.on_trigger.lock().unwrap())();
        }
    }
}

/// Serves `card` on port 4511 with the given DATA message limit.
pub(super) fn serve_card(router: &Arc<Router>, card: Arc<MockCard>, max_msg: u16) {
    let opts = xfer::ServeOptions { max_msg, zstd: false, min_gap: Duration::ZERO, tick: Duration::from_millis(2) };
    xfer::serve(router, SD_FILE_STREAM_PORT, card, opts, None).unwrap();
}

fn link() -> (Arc<Router>, Arc<Router>) {
    let gs = Arc::new(Router::new(RouterConfig::default().with_hostname("GS").with_static_address(0x2001)));
    let daq = Arc::new(Router::new(RouterConfig::default().with_hostname("DAQ").with_static_address(0x2002)));
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

fn serve(daq: &Arc<Router>, board: Arc<MockCard>) {
    serve_card(daq, board, 1024);
}

/// One pass of a router's queues for a test pump. A station run the test
/// aborted (a restart) drops its stream receiver; a frame still in flight to
/// it then fails its handler, as it does on a real station, which drops it.
/// The pump must carry on like the station's router task, not die (killing
/// every later run of the test, more often the more loaded the host).
pub(super) fn process(router: &Router) {
    match router.process_all_queues() {
        Ok(_) | Err(sedsnet::TelemetryError::HandlerError("SD stream receiver closed")) => {}
        Err(error) => panic!("router pump: {error:?}"),
    }
}

pub(super) fn pump(gs: Arc<Router>, daq: Arc<Router>) -> (Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let task = tokio::spawn(async move {
        while flag.load(Ordering::Relaxed) {
            process(&gs);
            process(&daq);
            tokio::task::yield_now().await;
        }
    });
    (running, task)
}

/// As [`pump`], but nothing moves between the routers while `up` is false
/// (a lost link: both sides keep queueing).
pub(super) fn pump_link(gs: Arc<Router>, board: Arc<Router>, up: Arc<AtomicBool>) -> (Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let task = tokio::spawn(async move {
        while flag.load(Ordering::Relaxed) {
            if up.load(Ordering::SeqCst) {
                process(&gs);
                process(&board);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });
    (running, task)
}

pub(super) fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn open_gate() -> Gate {
    Arc::new(|| None)
}

fn request(id: u64, root: &FsPath, name: &str) -> TransferRequest {
    TransferRequest::new(id, Board::DaqBoard, 0, name, root.into(), 10_000_000)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_downloads_resumes_and_verifies_partial_tail() {
    let (gs, daq) = link();
    let launch = pattern(3 * 512 + 77, 1);
    let utc = pattern(900, 9);
    let board = Arc::new(MockCard::new(vec![(0, "DAQ_LAUNCH_1_000.CSV", launch.clone()), (1, "DAQ_1_000.CSV", utc)]));
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

/// A board that takes longer than HASH_RETRY to hash the file (the DAQ with an
/// 18 MB launch file, r7) answers BUSY meanwhile; the station waits for it
/// instead of cancelling, which restarted the hash on every retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_hash_is_waited_for_not_cancelled() {
    let (gs, daq) = link();
    let launch = pattern(5 * 512 + 3, 4);
    let board = Arc::new(MockCard::new(vec![(0, "DAQ_LAUNCH_2_000.CSV", launch.clone())]));
    *board.hash_takes.lock().unwrap() = Some(Duration::from_secs(5));
    serve(&daq, board.clone());
    let (running, task) = pump(gs.clone(), daq.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let root = TestDirectory::new();
    let (final_path, _) = local_paths(&root.0, Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_2_000.CSV");
    let (id, cancel) = manager.create_job(Board::DaqBoard, "LAUNCH", "DAQ_LAUNCH_2_000.CSV").unwrap();
    transfer(manager, gs.clone(), &request(id, &root.0, "DAQ_LAUNCH_2_000.CSV"), &open_gate(), &cancel)
        .await
        .unwrap();
    assert_eq!(manager.get(id).unwrap().phase, Phase::Complete);
    assert_eq!(std::fs::read(&final_path).unwrap(), launch);
    assert_eq!(board.hash_restarts.load(Ordering::SeqCst), 0, "no CANCEL while the board hashes");
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_keeps_partial_and_a_later_start_resumes_it() {
    let (gs, daq) = link();
    let data = pattern(400 * 1024, 3);
    let cancel = Arc::new(AtomicBool::new(false));
    let trip = cancel.clone();
    let board = Arc::new(MockCard::new(vec![(0, "F.CSV", data.clone())]));
    board.trigger(64 * 1024, move || trip.store(true, Ordering::SeqCst));
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
    assert!(kept > 0 && kept < data.len() as u64, "kept {kept}: whole verified blocks only");
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
    let data = pattern(400 * 1024, 5);
    let blocked = Arc::new(AtomicBool::new(false));
    let trip = blocked.clone();
    let board = Arc::new(MockCard::new(vec![(0, "F.CSV", data.clone())]));
    board.trigger(32 * 1024, move || trip.store(true, Ordering::SeqCst));
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
    // The FC has only its card root (and is blocked here anyway).
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
    assert!(start_job(manager, gs.clone(), Board::RFBoard, "ROOT", "F.CSV", root.0.clone(), open_gate()).is_err());
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

// ------------------------------------------------- DAQ lead-up background fetch

const T0: i64 = 1_800_000_000_000;
const CSV_HEADER: &str = "timestamp_ms,monotonic_ms,sensor,value,raw_adc_code,raw_value,calibrated_value,time_source,adc_temperature_c,adc_temperature_code,uptime_ticks,uptime_frequency_hz,record_sequence\n";

/// DAQ CSV rows every `period` ms over `[from, to)` (network time).
fn daq_csv(from: i64, to: i64, period: i64) -> Vec<u8> {
    let mut text = format!("{CSV_HEADER}# calibration,kg1000,2.5,0\n");
    let mut ts = from;
    let mut seq = 0;
    while ts < to {
        text.push_str(&format!("{ts},{},kg1000_kg,,123,0.5,1.25,network,25.0,100,{},1000,{seq}\n", ts - T0 + 600_000, ts - T0 + 600_000));
        ts += period;
        seq += 1;
    }
    text.into_bytes()
}

/// A saved launch capture around T0, the DAQ launch file attached to it
/// (which queues the lead-up job). The launch file starts at acceptance.
async fn capture_with_daq_launch(root: &FsPath, launch_name: &str) -> PathBuf {
    let capture = crate::recording_store::Capture {
        serial: 1,
        kind: crate::recording_store::EventKind::Launch,
        key: "flight".into(),
        label: "Launch".into(),
        event_start_ms: T0,
        window_start_ms: T0 - 600_000,
        event_end_ms: Some(T0 + 120_000),
        post_roll_ms: 0,
        max_end_ms: T0 + 7_200_000,
        result: Some("Landed".into()),
        operator: None,
        station_mode: "normal".into(),
        provenance: None,
        details: serde_json::Value::Null,
        materializing: false,
    };
    let saved = crate::recording_store::materialize(root, &capture, T0 + 200_000).await.unwrap();
    let launch = root.join("incoming").join(launch_name);
    std::fs::create_dir_all(launch.parent().unwrap()).unwrap();
    std::fs::write(&launch, daq_csv(T0 - 10_000, T0 + 20_000, 100)).unwrap();
    crate::recording_store::board_sd::offer_file(root, &launch, launch_name, "network_download").unwrap();
    saved.folder
}

fn leadup_rows(folder: &FsPath, launch_name: &str) -> Vec<i64> {
    let lead = folder.join("board_sd/DAQ").join(format!("{}.leadup.csv", launch_name.trim_end_matches(".CSV")));
    std::fs::read_to_string(lead)
        .unwrap()
        .lines()
        .filter_map(crate::recording_store::board_sd::network_time)
        .collect()
}

#[test]
fn block_times_skip_partial_lines_and_command_traffic_quiets_the_link() {
    let block = b"0,partial\n1800000000100,1,x,,,,,network,,,1,1000,1\n1800000000200,2,x,,,,,network,,,2,1000,2\n18000000003";
    assert_eq!(leadup::block_times(block, false), (Some(T0 + 100), Some(T0 + 200)));
    assert_eq!(leadup::block_times(b"timestamp_ms,x\n", true), (None, None));
    leadup::note_command_traffic();
    assert_eq!(leadup::command_quiet().as_deref(), Some("commands were sent recently"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_session_file_yields_only_the_lead_up_window_and_resumes_after_a_drop() {
    // Slow enough (DATA B/s) that the link drop lands between blocks.
    leadup::TEST_RATE.store(2_000, Ordering::Relaxed);
    leadup::TEST_SMALL_FILE_BYTES.store(64 * 1024, Ordering::Relaxed);
    let root = TestDirectory::new();
    let launch_name = "DAQ_LAUNCH_1800000000000_000.CSV";
    let folder = capture_with_daq_launch(&root.0, launch_name).await;
    let jobs = leadup::load(&root.0);
    assert_eq!(jobs.len(), 1, "attaching the DAQ launch file queues its lead-up");
    assert_eq!((jobs[0].from_ms, jobs[0].to_ms), (T0 - 70_000, T0 - 10_000), "ends at the launch file's first row");
    // The session that ended at Launch acceptance: ten minutes of rows, far
    // larger than the small-file limit; an older session ended long before.
    let session = daq_csv(T0 - 610_000, T0 - 10_000, 100);
    assert!(session.len() as u64 > 4 * 64 * 1024);
    let old = daq_csv(T0 - 7_200_000, T0 - 7_100_000, 1_000);
    let (gs, daq) = link();
    let gate_open = Arc::new(AtomicBool::new(true));
    let flip = gate_open.clone();
    let board = Arc::new(MockCard::new(vec![
        (0, launch_name, daq_csv(T0 - 10_000, T0 + 20_000, 100)),
        (1, "DAQ_1799999390000_000.CSV", session.clone()),
        (2, "DAQ_0_000.CSV", old),
    ]));
    // Drop the link part-way through the window fetch (after the tails, the
    // header and the search, a few KB into the window).
    board.trigger(40 * 1024, move || flip.store(false, Ordering::SeqCst));
    serve(&daq, board.clone());
    let (running, task) = pump_link(gs.clone(), daq.clone(), gate_open.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let gate_flag = gate_open.clone();
    let gate: Gate = Arc::new(move || (!gate_flag.load(Ordering::SeqCst)).then(|| "link busy".to_owned()));
    let job = leadup::load(&root.0).remove(0);
    let first = leadup::step(manager, &root.0, gs.clone(), &job, &gate).await;
    assert!(matches!(first, Err(StreamError::Gate(_))), "{first:?}");
    let paused = leadup::load(&root.0).remove(0);
    assert_eq!(paused.source.as_deref(), Some("DAQ_1799999390000_000.CSV"));
    assert!(paused.start_offset.is_some() && !paused.done);
    let reads_before = board.served.load(Ordering::SeqCst);
    // Link back: the job resumes from what it already has.
    gate_open.store(true, Ordering::SeqCst);
    leadup::step(manager, &root.0, gs.clone(), &paused, &gate).await.unwrap();
    let done = leadup::load(&root.0).remove(0);
    assert!(done.done, "{done:?}");
    assert_eq!(done.phase, "complete");
    // Only the window was read, never the whole session file.
    let read_bytes = board.served.load(Ordering::SeqCst);
    assert!(read_bytes < session.len() as u64 / 3, "read {read_bytes} of {}", session.len());
    let _ = reads_before;
    // Exactly the minute before the launch file, no overlap, no duplicates.
    let rows = leadup_rows(&folder, launch_name);
    assert_eq!(rows.len(), 600);
    assert_eq!((rows[0], *rows.last().unwrap()), (T0 - 70_000, T0 - 10_100));
    assert!(rows.windows(2).all(|w| w[1] == w[0] + 100));
    let lead = std::fs::read_to_string(folder.join("board_sd/DAQ/DAQ_LAUNCH_1800000000000_000.leadup.csv")).unwrap();
    assert!(lead.starts_with(CSV_HEADER) && lead.contains("# calibration"));
    let manifest = crate::recording_store::read_manifest(&folder).unwrap();
    assert_eq!(manifest.board_sd[0].lead_up_rows, 600);
    // Large source: only the window is kept; scratch files are gone.
    assert!(completed_file(&root.0.join("sd-downloads"), "DAQ", "UTC", "DAQ_1799999390000_000.CSV").is_none());
    let scratch: Vec<_> = std::fs::read_dir(root.0.join("sd-downloads/DAQ/leadup")).unwrap().flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n != "jobs.json").collect();
    assert!(scratch.is_empty(), "{scratch:?}");
    assert!(!leadup::pending(&root.0));
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_small_session_file_is_downloaded_whole_and_kept() {
    // Slow enough (DATA B/s) that the link drop lands between blocks.
    leadup::TEST_RATE.store(2_000, Ordering::Relaxed);
    let root = TestDirectory::new();
    let launch_name = "DAQ_LAUNCH_1800000000000_001.CSV";
    let folder = capture_with_daq_launch(&root.0, launch_name).await;
    // A short session (two minutes at 1 Hz) is far under the small-file limit.
    let session = daq_csv(T0 - 120_000, T0 - 10_000, 1_000);
    let (gs, daq) = link();
    let board = Arc::new(MockCard::new(vec![(1, "DAQ_1799999880000_000.CSV", session.clone())]));
    serve(&daq, board);
    let (running, task) = pump(gs.clone(), daq.clone());
    let manager: &'static Manager = Box::leak(Box::new(Manager::default()));
    let job = leadup::load(&root.0).remove(0);
    leadup::step(manager, &root.0, gs.clone(), &job, &open_gate()).await.unwrap();
    assert!(leadup::load(&root.0)[0].done, "{:?}", leadup::load(&root.0));
    let kept = completed_file(&root.0.join("sd-downloads"), "DAQ", "UTC", "DAQ_1799999880000_000.CSV").unwrap();
    assert_eq!(std::fs::read(kept).unwrap(), session, "small source kept whole");
    let rows = leadup_rows(&folder, launch_name);
    assert_eq!(rows.len(), 60);
    assert_eq!((rows[0], *rows.last().unwrap()), (T0 - 70_000, T0 - 11_000));
    running.store(false, Ordering::Relaxed);
    task.await.unwrap();
}
