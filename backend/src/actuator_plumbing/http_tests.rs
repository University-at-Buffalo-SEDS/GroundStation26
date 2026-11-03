//! Actual HTTP handlers and packed SDK admission. AB GPIO/NVM is independently
//! qualified in firmware; this peer supplies only correlated protocol readback.
use super::*;
use sedsnet::router::{EndpointHandler, Router, RouterConfig, RouterSideOptions};
use std::sync::{
    Condvar, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use tower::ServiceExt;

struct Hook {
    path: PathBuf,
    after: bool,
    pause: bool,
    entered: bool,
    release: bool,
    fail: bool,
}
static HOOK: OnceLock<(Mutex<Option<Hook>>, Condvar)> = OnceLock::new();
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub(super) fn save_hook(path: &Path, after: bool) -> Result<(), String> {
    let (lock, cv) = HOOK.get_or_init(|| (Mutex::new(None), Condvar::new()));
    let mut hook = lock.lock().unwrap();
    if !hook
        .as_ref()
        .is_some_and(|h| h.path == path && h.after == after)
    {
        return Ok(());
    }
    hook.as_mut().unwrap().entered = true;
    cv.notify_all();
    while hook.as_ref().is_some_and(|h| h.pause && !h.release) {
        let (next, timeout) = cv
            .wait_timeout(hook, std::time::Duration::from_secs(5))
            .unwrap();
        hook = next;
        if timeout.timed_out() {
            return Err("Test pause timed out safely".into());
        }
    }
    if hook.as_ref().unwrap().fail {
        Err("Injected persistence boundary failure".into())
    } else {
        Ok(())
    }
}
static MOTION_CUT: OnceLock<Mutex<Option<(PathBuf, u8)>>> = OnceLock::new();
pub(super) fn motion_fault_cut(path: &Path, stage: u8) -> Result<(), String> {
    if MOTION_CUT
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|(expected, at)| expected == path && *at == stage)
    {
        return Err(format!("Injected motion history prefix cut{stage}"));
    }
    Ok(())
}
struct Pause;
impl Pause {
    fn install(path: &Path, after: bool, pause: bool, fail: bool) -> Self {
        let (lock, _) = HOOK.get_or_init(|| (Mutex::new(None), Condvar::new()));
        *lock.lock().unwrap() = Some(Hook {
            path: path.into(),
            after,
            pause,
            entered: false,
            release: false,
            fail,
        });
        Self
    }
    async fn entered(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if HOOK
                    .get()
                    .unwrap()
                    .0
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .entered
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual worker reached persistence hook");
    }
    fn release(&self) {
        let (lock, cv) = HOOK.get().unwrap();
        if let Some(h) = lock.lock().unwrap().as_mut() {
            h.release = true;
        }
        cv.notify_all();
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct Fixture {
    pub(crate) state: Arc<AppState>,
    root: PathBuf,
    pub(crate) host: Arc<Router>,
    board: Router,
    pub(crate) cmd_rx: Option<tokio::sync::mpsc::Receiver<crate::types::TelemetryCommand>>,
    db_rx: Option<tokio::sync::mpsc::Receiver<crate::DbQueueItem>>,
    clock: Arc<AtomicU64>,
    host_side: usize,
    board_side: usize,
    outgoing: Arc<Mutex<Vec<Vec<u8>>>>,
    incoming: Arc<Mutex<Vec<Vec<u8>>>>,
    requests: Arc<Mutex<Vec<Packet>>>,
    handled: usize,
    host_worker_running: bool,
    worker_wall: Arc<Mutex<Option<std::time::Instant>>>,
    mode: u8,
    result: u8,
    outputs: u32,
    motion: Option<Vec<u8>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.request_shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    pub(crate) async fn new() -> Self {
        Self::new_with_history(None).await
    }
    async fn new_with_history(raw: Option<&[u8]>) -> Self {
        let root =
            std::env::temp_dir().join(format!("plumbing-http-{:016x}", rand::random::<u64>()));
        fs::create_dir(&root).unwrap();
        if let Some(raw) = raw {
            let history = root.join("motion.json");
            fs::write(&history, raw).unwrap();
            fs::File::open(&history).unwrap().sync_all().unwrap();
            fs::File::open(&root).unwrap().sync_all().unwrap();
        }
        let auth = root.join("auth.json");
        fs::write(&auth,json!({"users":[
          {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":false}},
          {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}},
          {"username":"limited","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true},"command_access":{"allowed_commands":["Abort"]}}
        ]}).to_string()).unwrap();
        let mut state = crate::state::tests::test_app_state().await;
        // The held-authorization regression must own every pool connection;
        // normal SqlitePool defaults may grow during broad parallel tests.
        let auth_db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
        let (db_tx, _db_rx) = tokio::sync::mpsc::channel(32);
        {
            let s = Arc::get_mut(&mut state).unwrap();
            s.cmd_tx = cmd_tx;
            s.db_queue_tx = db_tx;
            s.auth_db = auth_db;
            s.auth = Arc::new(crate::auth::AuthManager::new(auth));
            let plumbing = Arc::get_mut(&mut s.plumbing_drive).unwrap();
            plumbing.path = root.join("drive.json");
            plumbing.motion_journal = motion_journal::Journal::new(root.join("motion.json"));
        }
        state.set_local_flight_state(FlightState::Idle);
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        for (user, commands) in [("viewer", 0), ("operator", 1), ("limited", 1)] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,?,0,9999999999999)").bind(user).bind(user).bind(commands).execute(&state.auth_db).await.unwrap();
        }
        let clock = Arc::new(AtomicU64::new(0));
        let worker_wall: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
        let host_wall = worker_wall.clone();
        let tick = clock.clone();
        let weak = Arc::downgrade(&state);
        let owner = Arc::as_ptr(&state) as usize;
        let host = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    let state = weak.upgrade().unwrap();
                    receive_status(&state, packet);
                    receive_motion_status(&state, packet);
                    crate::board_recovery::receive_status(&state, packet);
                    if packet.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS")
                    {
                        crate::telemetry_task::apply_umbilical_status_at_ingress(&state, packet);
                    }
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || {
                crate::gse::nonwaiting_admission_tests::clock_pause(owner);
                let wall = host_wall.lock().unwrap();
                tick.load(Ordering::Relaxed).saturating_add(wall.as_ref().map_or(0, |start| start.elapsed().as_millis() as u64))
            }),
        ));
        host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_563_200_000,
        ))
        .unwrap();
        assert!(state.topology_router.set(host.clone()).is_ok());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tick = clock.clone();
        let board_wall = worker_wall.clone();
        let handlers = ["BOARD_RECOVERY_AB", "ACTUATOR_BOARD"].map(|name| {
            let captured = requests.clone();
            EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint(name),
                move |packet| {
                    captured.lock().unwrap().push(packet.clone());
                    Ok(())
                },
            )
        });
        let board = Router::new_with_clock(
            RouterConfig::new(handlers).with_sender("AB"),
            Box::new(move || {
                let wall = board_wall.lock().unwrap();
                tick.load(Ordering::Relaxed).saturating_add(wall.as_ref().map_or(0, |start| start.elapsed().as_millis() as u64))
            }),
        );
        let outgoing = Arc::new(Mutex::new(Vec::new()));
        let output = outgoing.clone();
        let host_side = host.add_side_packed_with_options(
            "wire",
            move |wire| {
                output.lock().unwrap().push(wire.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        let incoming = Arc::new(Mutex::new(Vec::new()));
        let output = incoming.clone();
        let board_side = board.add_side_packed_with_options(
            "wire",
            move |wire| {
                output.lock().unwrap().push(wire.to_vec());
                Ok(())
            },
            RouterSideOptions {
                reliable_enabled: true,
                ..Default::default()
            },
        );
        host.rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "AB",
                1,
                &[
                    crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB"),
                    crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                ],
            )
            .unwrap(),
            host_side,
        )
        .unwrap();
        board
            .rx_from_side(
                &sedsnet::discovery::build_discovery_announce(
                    "GS",
                    1,
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                )
                .unwrap(),
                board_side,
            )
            .unwrap();
        Self {
            state,
            root,
            host,
            board,
            cmd_rx: Some(_cmd_rx),
            db_rx: Some(_db_rx),
            clock,
            host_side,
            board_side,
            outgoing,
            incoming,
            requests,
            handled: 0,
            host_worker_running: false,
            worker_wall,
            mode: 0,
            result: 0,
            outputs: 16,
            motion: None,
        }
    }
    async fn request(
        state: Arc<AppState>,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> Response {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        crate::web::router(state, String::new())
            .oneshot(
                request
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    pub(crate) async fn http(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> Response {
        Self::request(self.state.clone(), method, path, token, body).await
    }
    fn set_host_worker_running(&mut self, running: bool) {
        let mut wall = self.worker_wall.lock().unwrap();
        if let Some(start) = wall.take() {
            self.clock.fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        }
        if running {
            *wall = Some(std::time::Instant::now());
        }
        self.host_worker_running = running;
    }
    pub(crate) fn pump(&mut self) {
        for _ in 0..80 {
            // The real owner uses the shared elapsed-wall clock. Polling
            // advances virtual time only when no real SDK worker owns it.
            if !self.host_worker_running {
                self.clock.fetch_add(1, Ordering::Relaxed);
            }
            if !self.host_worker_running {
                self.host.process_all_queues_with_timeout(0).unwrap();
            }
            for bytes in std::mem::take(&mut *self.outgoing.lock().unwrap()) {
                self.board
                    .rx_packed_from_side(&bytes, self.board_side)
                    .unwrap();
            }
            self.board.process_all_queues_with_timeout(0).unwrap();
            let captured = self.requests.lock().unwrap().clone();
            for packet in &captured[self.handled..] {
                let p = packet.payload();
                let recovery = packet.data_type()
                    == crate::telemetry_schema::data_type("BOARD_RECOVERY_REQUEST");
                if !recovery
                    && packet.data_type() != crate::telemetry_schema::data_type(REQUEST_TYPE)
                {
                    continue;
                }
                assert_eq!(p.len(), 32);
                if !recovery && p[0] == 2 {
                    assert_eq!(&p[..4], &[2, 0, 0, 0]);
                    if let Some(mut proof) = self.motion.clone() {
                        proof[4..12].copy_from_slice(&p[4..12]);
                        proof[24..32].copy_from_slice(&p[24..32]);
                        // Actual public SDK typed Static56 route, not only a
                        // hand-constructed Packet/decoder unit control.
                        self.board
                            .log_queue_ts(
                                crate::telemetry_schema::data_type(motion_wire::TYPE),
                                packet.timestamp(),
                                &proof,
                            )
                            .unwrap();
                    }
                    continue;
                }
                let mut bytes = [0u8; 40];
                bytes[0] = 1;
                bytes[4..12].copy_from_slice(&p[4..12]);
                bytes[12..20].copy_from_slice(&42u64.to_le_bytes());
                bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
                let name = if recovery {
                    bytes[2] = FlightState::Idle as u8;
                    bytes[3] = 8 | 16 | 4 | if self.outputs == 0 { 2 } else { 0 };
                    bytes[28..32].copy_from_slice(&self.outputs.to_le_bytes());
                    bytes[32..40].copy_from_slice(&p[24..32]);
                    "BOARD_STATE_STATUS"
                } else {
                    if p[1] == 1 {
                        self.mode = p[2];
                        self.outputs = 0;
                    }
                    bytes[1] = self.result;
                    bytes[2] = self.mode;
                    bytes[3] = 5;
                    bytes[24..32].copy_from_slice(&p[24..32]);
                    STATUS_TYPE
                };
                self.board
                    .tx_queue(
                        Packet::new(
                            crate::telemetry_schema::data_type(name),
                            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                            "AB",
                            packet.timestamp(),
                            Arc::from(bytes),
                        )
                        .unwrap(),
                    )
                    .unwrap();
            }
            self.handled = captured.len();
            for bytes in std::mem::take(&mut *self.incoming.lock().unwrap()) {
                self.host
                    .rx_packed_from_side(&bytes, self.host_side)
                    .unwrap();
            }
        }
    }
    pub(crate) async fn prime(&mut self) {
        assert_eq!(
            self.http(
                "POST",
                "/api/actuator/plumbing/query",
                Some("viewer"),
                json!({})
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            self.http(
                "POST",
                "/api/board-recovery/query",
                Some("viewer"),
                json!({"board":"AB"})
            )
            .await
            .status(),
            StatusCode::OK
        );
        self.board
            .tx_queue(
                Packet::new_with_nonce(
                    crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                    &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                    "AB",
                    self.host.network_time_ms().unwrap(),
                    800,
                    Arc::from([8, 0]),
                )
                .unwrap(),
            )
            .unwrap();
        self.pump();
        assert!(crate::board_recovery::actuator_idle_proof(&self.state).is_some());
        assert!(
            self.state
                .plumbing_drive
                .inner
                .lock()
                .unwrap()
                .observed
                .is_some()
        );
    }
    pub(crate) fn body(&self, mode: &str, release: bool) -> Value {
        json!({"mode":mode,"expected_generation":"000000000000002a","expected_epoch":1,"backend_instance":format!("{:016x}",self.state.plumbing_drive.issuer),"expected_revision":self.state.plumbing_drive.inner.lock().unwrap().desired.revision,"expected_reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&self.state)),"release_stepper":release})
    }
    fn sets(&self) -> Vec<Packet> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| {
                p.data_type() == crate::telemetry_schema::data_type(REQUEST_TYPE)
                    && p.payload()[1] == 1
            })
            .cloned()
            .collect()
    }
}

#[tokio::test]
async fn public_http_auth_permissions_explicit_release_and_confirmed_durable_set() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.state.record_software_command_if_fresh(
        &crate::types::TelemetryCommand::RetractPlumbing,
        1234,
        500,
    );
    f.state
        .record_command_accepted(&crate::types::TelemetryCommand::RetractPlumbing, 1234);
    let before_dedup = f.state.last_command_ms.lock().unwrap().clone();
    let body = f.body("auxiliary", true);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            None,
            body.clone()
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("viewer"),
            body.clone()
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("limited"),
            body
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!f.state.plumbing_drive.path.exists());
    f.prime().await;
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("stepper", false)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert!(!f.state.plumbing_drive.path.exists());
    for (mode, release) in [("h_bridge", false), ("auxiliary", false)] {
        assert_eq!(
            f.http(
                "POST",
                "/api/actuator/plumbing/settings",
                Some("operator"),
                f.body(mode, release)
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        assert!(!f.state.plumbing_drive.path.exists());
    }
    assert_eq!(
        *f.state.last_command_ms.lock().unwrap(),
        before_dedup,
        "unauthorized, unchanged and invalid settings preserve dedup/history"
    );
    let origin = f.host.network_time_ms().unwrap();
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", true)
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    assert!(
        Desired::load(&f.state.plumbing_drive.path)
            .unwrap()
            .is_some()
    );
    assert!(motion_ready(&f.state).is_err());
    f.pump();
    let sets = f.sets();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].timestamp(), origin);
    assert_eq!(sets[0].payload()[2], 2);
    assert!(motion_ready(&f.state).is_ok());
    // Fresh proof for the already-confirmed auxiliary mode permits an honest
    // no-op, preserving durable bytes and the new-mode dedup/history.
    f.clock.fetch_add(1_000, Ordering::Relaxed);
    f.outputs = 0;
    f.prime().await;
    assert!(f.state.record_software_command_if_fresh(
        &crate::types::TelemetryCommand::RetractPlumbing,
        2345,
        500
    ));
    let before_noop = f.state.last_command_ms.lock().unwrap().clone();
    let saved = fs::read(&f.state.plumbing_drive.path).unwrap();
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", false)
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(*f.state.last_command_ms.lock().unwrap(), before_noop);
    assert_eq!(fs::read(&f.state.plumbing_drive.path).unwrap(), saved);
    f.pump();
    assert_eq!(f.sets().len(), 1);
    assert_eq!(
        f.state.operator_dispatch_inflight.load(Ordering::Acquire),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_http_worker_keeps_ownership_ingress_and_stop_responsive() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    let pause = Pause::install(&f.state.plumbing_drive.path, false, true, false);
    let state = f.state.clone();
    let body = f.body("auxiliary", true);
    let request = tokio::spawn(async move {
        Fixture::request(
            state,
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            body,
        )
        .await
    });
    pause.entered().await;
    request.abort();
    assert_eq!(
        f.state.operator_dispatch_inflight.load(Ordering::Acquire),
        1
    );
    assert!(
        f.state.plumbing_drive.inner.try_lock().is_ok(),
        "no metadata mutex across IO"
    );
    assert_eq!(
        f.http(
            "GET",
            "/api/actuator/plumbing/status",
            Some("viewer"),
            Value::Null
        )
        .await
        .status(),
        StatusCode::OK
    );
    f.pump();
    assert_eq!(
        f.http("POST", "/api/command", Some("operator"), json!("Abort"))
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    assert!(f.state.manual_abort_intent.pending_epoch().is_some());
    pause.release();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while f.state.operator_dispatch_inflight.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    f.pump();
    assert!(f.sets().is_empty());
    assert!(f.state.manual_abort_intent.pending_epoch().is_some());
    assert!(motion_ready(&f.state).is_err());
}

#[tokio::test]
async fn directory_flush_ambiguity_is_507_visible_and_never_admits_set() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    f.state.record_software_command_if_fresh(
        &crate::types::TelemetryCommand::RetractPlumbing,
        1234,
        500,
    );
    let before_dedup = f.state.last_command_ms.lock().unwrap().clone();
    let _failure = Pause::install(&f.state.plumbing_drive.path, true, false, true);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", true)
        )
        .await
        .status(),
        StatusCode::INSUFFICIENT_STORAGE
    );
    assert_eq!(
        Desired::load(&f.state.plumbing_drive.path)
            .unwrap()
            .unwrap()
            .mode,
        2
    );
    f.pump();
    assert!(f.sets().is_empty());
    assert_eq!(
        *f.state.last_command_ms.lock().unwrap(),
        before_dedup,
        "failed durability never clears dedup"
    );
    let inner = f.state.plumbing_drive.inner.lock().unwrap();
    assert!(!inner.durable);
    assert!(inner.storage_error.is_some());
    drop(inner);
    assert!(motion_ready(&f.state).is_err());
}

#[tokio::test]
async fn actual_sqlite_auth_wait_cannot_refresh_settings_origin_or_revoked_permission() {
    use futures::FutureExt;
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    let held = f.state.auth_db.acquire().await.unwrap();
    assert_eq!(f.state.auth_db.num_idle(), 0);
    let body = f.body("auxiliary", true);
    let request = f.http(
        "POST",
        "/api/actuator/plumbing/settings",
        Some("operator"),
        body,
    );
    tokio::pin!(request);
    assert!(
        request.as_mut().now_or_never().is_none(),
        "actual polled authorization waits for held SQLite connection"
    );
    f.clock.fetch_add(6_000, Ordering::Relaxed);
    drop(held);
    assert_eq!(request.await.status(), StatusCode::CONFLICT);
    assert!(!f.state.plumbing_drive.path.exists());
    sqlx::query("UPDATE auth_sessions SET session_type='revoked',can_send_commands=0 WHERE token='operator'").execute(&f.state.auth_db).await.unwrap();
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", true)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert!(!f.state.plumbing_drive.path.exists());
}

#[tokio::test]
async fn metadata_io_retry_is_explicit_and_matching_actual_idle_proof_is_required() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.mode = 255;
    f.result = 6;
    f.outputs = 0;
    f.prime().await;
    assert!(motion_ready(&f.state).is_err());
    assert!(view(&f.state, true)["can_configure"].as_bool().unwrap());
    assert!(!f.state.plumbing_drive.path.exists());
    let mut stale = f.body("auxiliary", true);
    stale["expected_epoch"] = json!(2);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            stale
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert!(!f.state.plumbing_drive.path.exists());
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", true)
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    f.result = 0;
    f.pump();
    assert_eq!(f.sets().len(), 1);
    assert!(motion_ready(&f.state).is_ok());
}

#[cfg(not(feature = "hitl_mode"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_motion_queue_cannot_cross_mode_commit_and_fresh_motion_keeps_original_stamp() {
    // The testing feature's synthetic FlightSim owns commands; disable it in a
    // fresh process so normal and test_fire both exercise the production owner.
    const NAME: &str = "actuator_plumbing::http_tests::public_motion_queue_cannot_cross_mode_commit_and_fresh_motion_keeps_original_stamp";
    if cfg!(feature = "testing") && std::env::var_os("GS_PLUMBING_OWNER_CHILD").is_none() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("GS_PLUMBING_OWNER_CHILD", "1")
            .env("GS_SIM_MODE", "0")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    let old_path = crate::gse::armed_fixture_for_test(&f.state, &f.host);
    for side in f
        .host
        .export_topology()
        .routes
        .iter()
        .filter(|s| s.side_name == "test_fixture_other_boards")
    {
        f.host.remove_side(side.side_id).unwrap();
    }
    crate::gse::reset_plumbing_qd_for_test(&f.state);
    f.host
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "FC",
                1,
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            )
            .unwrap(),
            f.host_side,
        )
        .unwrap();
    assert_eq!(
        f.http(
            "POST",
            "/api/command",
            Some("operator"),
            json!("RetractPlumbing")
        )
        .await
        .status(),
        StatusCode::OK
    );
    f.state
        .record_command_accepted(&crate::types::TelemetryCommand::RetractPlumbing, 1234);
    assert!(f.state.record_software_command_if_fresh(
        &crate::types::TelemetryCommand::Postinit,
        1234,
        500
    ));
    let before_generation = crate::gse::automatic_generation(&f.state);
    f.state.set_local_flight_state(FlightState::Idle);
    f.clock.fetch_add(1, Ordering::Relaxed);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            f.body("auxiliary", true)
        )
        .await
        .status(),
        StatusCode::ACCEPTED
    );
    let dedup_after_set = f
        .state
        .last_command_ms
        .lock()
        .unwrap()
        .get("dedup:RetractPlumbing")
        .copied();
    assert_eq!(
        f.state
            .last_command_ms
            .lock()
            .unwrap()
            .get("RetractPlumbing"),
        Some(&1234)
    );
    assert_eq!(
        f.state
            .last_command_ms
            .lock()
            .unwrap()
            .get("dedup:Postinit"),
        Some(&1234)
    );
    assert_eq!(
        crate::gse::automatic_generation(&f.state),
        before_generation
    );
    f.state.set_local_flight_state(FlightState::Armed);
    assert_eq!(
        f.http(
            "POST",
            "/api/command",
            Some("operator"),
            json!("RetractPlumbing")
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.state
            .last_command_ms
            .lock()
            .unwrap()
            .get("dedup:RetractPlumbing")
            .copied(),
        dedup_after_set,
        "pending settings attempt must not consume dedup"
    );
    f.pump();
    assert!(motion_ready(&f.state).is_ok());
    let path = crate::gse::armed_fixture_for_test(&f.state, &f.host);
    for side in f
        .host
        .export_topology()
        .routes
        .iter()
        .filter(|s| s.side_name == "test_fixture_other_boards")
    {
        f.host.remove_side(side.side_id).unwrap();
    }
    crate::gse::reset_plumbing_qd_for_test(&f.state);
    f.host
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "FC",
                2,
                &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            )
            .unwrap(),
            f.host_side,
        )
        .unwrap();
    // Restore the actual peer route after the generic armed fixture announced GB.
    f.host
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "AB",
                2,
                &[
                    crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                    crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB"),
                ],
            )
            .unwrap(),
            f.host_side,
        )
        .unwrap();
    let shutdown = f.state.shutdown_subscribe();
    f.set_host_worker_running(true);
    let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
        f.state.clone(),
        f.host.clone(),
        Vec::new(),
        f.cmd_rx.take().unwrap(),
        f.db_rx.take().unwrap(),
        shutdown,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    f.pump();
    assert_eq!(
        f.state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .last_motion_stamp,
        0,
        "old application-queued request must be retired before SDK dispatch"
    );
    assert!(!f.requests.lock().unwrap().iter().any(|p| p.data_type()
        == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
        && p.payload() == [8]));
    // New protocol prerequisite: actual fresh current-mode/safe-state proof,
    // not a synthetic timer or an arbitrary sleep/dedup delay.
    f.prime_motion_identity().await;
    // Immediate new-mode intent must not need a dedup delay.
    let before_request_stamp = f.host.network_time_ms().unwrap();
    let started = std::time::Instant::now();
    let mut warnings = f.state.warnings_tx.subscribe();
    let mut notifications = f.state.notifications_tx.subscribe();
    let journal_pause = Pause::install(f.state.plumbing_drive.motion_journal.path(), true, true, false);
    let fresh = f
        .http(
            "POST",
            "/api/command",
            Some("operator"),
            json!("RetractPlumbing"),
        )
        .await;
    assert_eq!(fresh.status(), StatusCode::OK);
    let body = axum::body::to_bytes(fresh.into_body(), 1024).await.unwrap();
    assert_ne!(
        &body[..],
        b"duplicate ignored",
        "old-mode dedup must not consume genuine immediate new-mode intent"
    );
    // Exercise the actual owned writer while repeatedly servicing packed
    // queues. Polling alone is not authority to expire the original command.
    let after_request_stamp = f.host.network_time_ms().unwrap();
    journal_pause.entered().await;
    let stamp = motion_journal::load(f.state.plumbing_drive.motion_journal.path()).unwrap().unwrap().original_utc_ms;
    assert!((before_request_stamp..=after_request_stamp).contains(&stamp));
    for _ in 0..80 {
        f.pump();
        tokio::task::yield_now().await;
    }
    assert!(f.state.plumbing_drive.motion_journal.pending());
    journal_pause.release();
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            f.pump();
            if f.requests.lock().unwrap().iter().any(|p| {
                p.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
                    && p.payload() == [8]
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if delivered.is_err() {
        while let Ok(warning) = warnings.try_recv() { eprintln!("PLUMBING_WARNING {:?}", warning.message); }
        while let Ok(notification) = notifications.try_recv() { eprintln!("PLUMBING_NOTIFICATION {:?}", serde_json::to_string(&notification)); }
        eprintln!("PLUMBING_TIMEOUT original={} now={} wall_ms={} pending={} journal={:?} motion_ready={:?} SDK={:?} proof={:?} outgoing={} incoming={} captured={}",stamp,f.host.network_time_ms().unwrap(),started.elapsed().as_millis(),f.state.plumbing_drive.motion_journal.pending(),f.state.plumbing_drive.motion_journal.snapshot(),motion_ready(&f.state),f.host.export_runtime_stats(),f.state.plumbing_drive.inner.lock().unwrap().motion_observed.as_ref().map(|row|(row.mode_revision,row.generation,row.after.wall.elapsed().as_millis())),f.outgoing.lock().unwrap().len(),f.incoming.lock().unwrap().len(),f.requests.lock().unwrap().len());
    }
    delivered.unwrap();
    let motions: Vec<_> = f
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|p| {
            p.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
                && p.payload() == [8]
        })
        .cloned()
        .collect();
    assert_eq!(motions.len(), 1);
    assert_eq!(motions[0].timestamp(), stamp);
    assert_eq!(
        f.http(
            "POST",
            "/api/command",
            Some("operator"),
            json!("ToggleAvBayUnderglow")
        )
        .await
        .status(),
        StatusCode::OK
    );
    f.state.request_shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    f.set_host_worker_running(false);
    #[cfg(not(feature = "test_fire_mode"))]
    {
        use gse_sequence::qd_wait::QdStatus;
        assert_eq!(
            crate::gse::plumbing_qd_probe_for_test(&f.state, 0).0,
            QdStatus::AwaitingAck,
            "SDK admission is not completion"
        );
        let (status, floor) = crate::gse::plumbing_qd_probe_for_test(&f.state, 45_000);
        assert_eq!(
            status,
            QdStatus::AwaitingAck,
            "new auxiliary deadline must outlive the old five-second ACK timeout"
        );
        // The firmware peer's electrical timer is tested in actual C. Here a
        // modeled elapsed monotonic clock and actual packed positive readback
        // exercise the GS completion/settle contract after public dispatch.
        for (nonce, on) in [(901, 0), (902, 1)] {
            f.board
                .tx_queue(
                    Packet::new_with_nonce(
                        crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
                        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
                        "AB",
                        floor + 45_001 + nonce,
                        nonce as u16,
                        Arc::from([8, on]),
                    )
                    .unwrap(),
                )
                .unwrap();
            f.pump();
            let status = crate::gse::plumbing_qd_probe_for_test(&f.state, 0).0;
            if on == 0 {
                assert_eq!(status, QdStatus::AwaitingAck);
            } else {
                assert!(matches!(status, QdStatus::Settling { .. }));
            }
        }
        assert!(matches!(
            crate::gse::plumbing_qd_probe_for_test(&f.state, 4_900).0,
            QdStatus::Settling { .. }
        ));
        assert_eq!(
            crate::gse::plumbing_qd_probe_for_test(&f.state, 200).0,
            QdStatus::Ready
        );
    }
    // Process shutdown has sealed ingress and joined the actual receiver above.
    // A new Stop must not be reported accepted when no owner can deliver it.
    let terminal_manual_epoch = f.state.deferred_abort.manual_epoch();
    assert_eq!(
        f.http("POST", "/api/command", Some("operator"), json!("Abort"))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(f.state.deferred_abort.manual_epoch(), terminal_manual_epoch);

    let _ = fs::remove_file(path);
    let _ = fs::remove_file(old_path);
}

#[tokio::test]
async fn actual_board_output_mask_never_ignores_bridge_signal_or_active_aux_bits() {
    let _serial = SERIAL.lock().await;
    for bit in [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 31] {
        let mut f = Fixture::new().await;
        f.outputs = 16 | (1u32 << bit);
        f.prime().await;
        assert_eq!(
            f.http(
                "POST",
                "/api/actuator/plumbing/settings",
                Some("operator"),
                f.body("auxiliary", true)
            )
            .await
            .status(),
            StatusCode::CONFLICT,
            "output bit {bit} must block release/persist/SET"
        );
        assert!(!f.state.plumbing_drive.path.exists());
        f.pump();
        assert!(f.sets().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_settings_time_expiring_during_file_io_never_becomes_fresh_sdk_set() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    let pause = Pause::install(&f.state.plumbing_drive.path, false, true, false);
    let state = f.state.clone();
    let body = f.body("auxiliary", true);
    let request = tokio::spawn(async move {
        Fixture::request(
            state,
            "POST",
            "/api/actuator/plumbing/settings",
            Some("operator"),
            body,
        )
        .await
    });
    pause.entered().await;
    f.clock.fetch_add(6_000, Ordering::Relaxed);
    pause.release();
    assert_eq!(request.await.unwrap().status(), StatusCode::CONFLICT);
    assert!(
        Desired::load(&f.state.plumbing_drive.path)
            .unwrap()
            .is_some()
    );
    f.pump();
    assert!(f.sets().is_empty());
    assert!(motion_ready(&f.state).is_err());
}

#[tokio::test]
async fn motion_v2_actual_public_sdk_separate_static56_requires_correlated_start_identity() {
    let _serial = SERIAL.lock().await;
    let mut fixture = Fixture::new().await;
    fixture.prime().await;
    let mut bytes = vec![0; 56];
    bytes[0] = 2;
    bytes[2] = 0;
    bytes[3] = 5;
    bytes[12..20].copy_from_slice(&42u64.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[34] = 2;
    bytes[35] = 1;
    bytes[40..48].copy_from_slice(&10_000u64.to_le_bytes());
    bytes[52..56].copy_from_slice(&1u32.to_le_bytes());
    fixture.motion = Some(bytes);
    request_motion_query(&fixture.state).unwrap();
    fixture.pump();
    let inner = fixture.state.plumbing_drive.inner.lock().unwrap();
    let proof = inner
        .motion_observed
        .as_ref()
        .expect("actual typed56 SDK callback");
    assert!(proof.value.completed_retract());
    assert_eq!(proof.value.original_utc_ms, 10_000);
    assert_eq!(proof.value.origin_epoch, 1);
    let view = motion_view(&fixture.state, &inner);
    assert_eq!(view["completed_minimum_delay_proof"], true);
    assert_eq!(
        view["ready"], false,
        "board proof alone is not durable host intent"
    );
    drop(inner);
    let before = fixture.requests.lock().unwrap().len();
    for _ in 0..20 {
        request_motion_query(&fixture.state).unwrap();
    }
    fixture.pump();
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        before,
        "one bounded explicit motion query per cooldown"
    );
}

impl Fixture {
    async fn prime_motion_identity(&mut self) {
        self.prime().await;
        let mut proof = vec![0; 56];
        proof[0] = 2;
        proof[2] = self.mode;
        proof[3] = 5;
        proof[12..20].copy_from_slice(&42u64.to_le_bytes());
        proof[20..24].copy_from_slice(&1u32.to_le_bytes());
        proof[48..52].copy_from_slice(&45_000u32.to_le_bytes());
        self.motion = Some(proof);
        // Explicit readiness may already have issued an old-mode query. A
        // new mode proof honors the real one-second query cooldown; no test
        // clears retained nonce/revision state or changes the production gate.
        if self.state.plumbing_drive.inner.lock().unwrap().motion_queries.back()
            .is_some_and(|query|query.sent.age().is_some_and(|age|age<1_000)) {
            tokio::time::sleep(std::time::Duration::from_millis(1_010)).await;
        }
        request_motion_query(&self.state).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2),async {
            loop {
                self.pump();
                let current = {
                    let inner=self.state.plumbing_drive.inner.lock().unwrap();
                    inner.motion_observed.as_ref().is_some_and(|row|
                        row.mode_revision==inner.desired.revision&&row.value.setting.mode.map(|mode|mode as u8)==Some(self.mode))
                };
                if current {break;}
                tokio::task::yield_now().await;
            }
        }).await.expect("actual current-mode nonce-correlated MotionV2 response");
    }

    fn retractions(&self) -> Vec<Packet> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|p| {
                p.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
                    && p.payload() == [8]
            })
            .cloned()
            .collect()
    }
    async fn wait_motion_worker(&mut self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while self.state.plumbing_drive.motion_journal.pending() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual owned motion worker terminal");
        self.pump();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_journal_actual_owned_worker_retains_claim_but_stop_and_other_controls_remain_responsive()
 {
    let _serial = SERIAL.lock().await;
    let mut fixture = Fixture::new().await;
    fixture.prime_motion_identity().await;
    let source = capture_motion_origin(
        &fixture.state,
        &crate::types::TelemetryCommand::RetractPlumbing,
    )
    .unwrap();
    let path = fixture
        .state
        .plumbing_drive
        .motion_journal
        .path()
        .to_owned();
    let pause = Pause::install(&path, false, true, false);
    crate::gse::queue_plumbing_with_stop_fence(&fixture.state, &fixture.host, Some(source))
        .unwrap();
    pause.entered().await;
    assert!(
        !path.exists(),
        "no SDK motion can precede durable pending intent"
    );
    assert_eq!(
        fixture
            .state
            .operator_dispatch_inflight
            .load(Ordering::Acquire),
        1
    );
    assert!(
        fixture.state.gse.try_lock().is_ok(),
        "no GSE lock held during journal IO"
    );
    fixture.pump();
    assert!(fixture.retractions().is_empty());
    let other = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        fixture.http(
            "POST",
            "/api/command",
            Some("operator"),
            json!("ToggleAvBayUnderglow"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(other.status(), StatusCode::OK);
    let stop = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        fixture.http("POST", "/api/command", Some("operator"), json!("Abort")),
    )
    .await
    .unwrap();
    assert_eq!(stop.status(), StatusCode::ACCEPTED);
    assert!(fixture.state.manual_abort_intent.pending_epoch().is_some());
    pause.release();
    fixture.wait_motion_worker().await;
    assert_eq!(
        fixture
            .state
            .operator_dispatch_inflight
            .load(Ordering::Acquire),
        0
    );
    assert!(
        fixture.retractions().is_empty(),
        "Stop supersedes the original worker before SDK motion ownership"
    );
    let retained = motion_journal::load(&path).unwrap().unwrap();
    assert_eq!(retained.original_utc_ms, source.source_ms.unwrap());
    assert_ne!(
        retained.manual_epoch,
        fixture.state.manual_abort_intent.epoch()
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_journal_actual_worker_durable_readback_precedes_original_sdk_motion_and_failures_are_closed()
 {
    let _serial = SERIAL.lock().await;
    for fail in [true, false] {
        let mut fixture = Fixture::new().await;
        fixture.prime_motion_identity().await;
        let source = capture_motion_origin(
            &fixture.state,
            &crate::types::TelemetryCommand::RetractPlumbing,
        )
        .unwrap();
        let path = fixture
            .state
            .plumbing_drive
            .motion_journal
            .path()
            .to_owned();
        let pause = Pause::install(&path, false, true, fail);
        crate::gse::queue_plumbing_with_stop_fence(&fixture.state, &fixture.host, Some(source))
            .unwrap();
        pause.entered().await;
        fixture.pump();
        assert!(fixture.retractions().is_empty());
        pause.release();
        fixture.wait_motion_worker().await;
        let (record, error) = fixture.state.plumbing_drive.motion_journal.snapshot();
        if fail {
            assert!(error.is_some());
            assert!(record.is_none());
            assert!(fixture.retractions().is_empty());
            assert!(
                !retraction_gate(&fixture.state).allowed,
                "a latched write failure cannot become unconfigured/ready"
            );
            assert!(
                crate::gse::queue_plumbing_with_stop_fence(
                    &fixture.state,
                    &fixture.host,
                    Some(source)
                )
                .is_err()
            );
        } else {
            assert!(error.is_none());
            let record = record.unwrap();
            assert_eq!(motion_journal::load(&path).unwrap(), Some(record));
            let motions = fixture.retractions();
            assert_eq!(motions.len(), 1);
            assert_eq!(motions[0].timestamp(), source.source_ms.unwrap());
        }
        assert_eq!(
            fixture
                .state
                .operator_dispatch_inflight
                .load(Ordering::Acquire),
            0
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_final_gate_uses_actual_minimum_proof_and_original_operation_not_source_utc_age() {
    #[cfg(feature="testing")]
    if std::env::var_os("GS_MOTION_SDK_FIXTURE_CHILD").is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","actuator_plumbing::http_tests::motion_final_gate_uses_actual_minimum_proof_and_original_operation_not_source_utc_age","--nocapture","--test-threads=1"])
            .env("GS_MOTION_SDK_FIXTURE_CHILD","1").env("GS_SIM_MODE","0").output().unwrap();
        assert!(output.status.success(),"{}{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        let child_stdout=String::from_utf8_lossy(&output.stdout);
        assert!(child_stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored"),
            "hardware SDK child must execute exactly one original control: {child_stdout}");
        print!("{child_stdout}");return;
    }
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime_motion_identity().await;
    assert!(
        !retraction_gate(&f.state).configured,
        "legacy enum alone is not configuration"
    );
    let old_revision =
        crate::board_abort::capture_launch_revision_named(&f.state, "Launch").unwrap();
    let origin =
        capture_motion_origin(&f.state, &crate::types::TelemetryCommand::RetractPlumbing).unwrap();
    crate::gse::queue_plumbing_with_stop_fence(&f.state, &f.host, Some(origin)).unwrap();
    f.wait_motion_worker().await;
    assert_eq!(f.retractions().len(), 1);
    let record = f.state.plumbing_drive.motion_journal.snapshot().0.unwrap();
    assert!(
        retraction_gate(&f.state).configured,
        "actual durable intent seals the configured condition"
    );
    let mut proof = f.motion.clone().unwrap();
    proof[34] = 2;
    proof[35] = 1;
    proof[40..48].copy_from_slice(&record.original_utc_ms.to_le_bytes());
    proof[52..56].copy_from_slice(&record.epoch.to_le_bytes());
    proof[48..52].copy_from_slice(&44_999u32.to_le_bytes());
    f.motion = Some(proof.clone());
    // UTC age is deliberately >45s. Only actual accepted-start remaining may
    // release the minimum; this modeled peer response represents early legacy
    // completion. Real ThreadX timing is independently qualified in AB C tests.
    f.clock.fetch_add(45_100, Ordering::Relaxed);
    // A continuously connected peer refreshes its discovery lease too; do not
    // accidentally turn this timer identity control into a route-expiry test.
    f.host
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "AB",
                1,
                &[
                    crate::telemetry_schema::endpoint("ACTUATOR_BOARD"),
                    crate::telemetry_schema::endpoint("BOARD_RECOVERY_AB"),
                ],
            )
            .unwrap(),
            f.host_side,
        )
        .unwrap();
    f.board
        .rx_from_side(
            &sedsnet::discovery::build_discovery_announce(
                "GS",
                1,
                &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            )
            .unwrap(),
            f.board_side,
        )
        .unwrap();
    // Host query cooldown uses real monotonic time in normal mode; advancing
    // only the SDK test clock must not bypass its one-second rate limit.
    tokio::time::sleep(std::time::Duration::from_millis(1_010)).await;
    request_motion_query(&f.state).unwrap();
    f.pump();
    let gate = retraction_gate(&f.state);
    #[cfg(not(feature = "hitl_mode"))]
    assert!(!gate.allowed);
    #[cfg(feature = "hitl_mode")]
    {
        assert!(
            gate.allowed,
            "known board-owned operation may bypass min45 in HITL"
        );
        f.state.toggle_hitl_safety_interlock(0);
        assert!(!retraction_gate(&f.state).allowed);
    }
    assert_eq!(
        retraction_gate(&f.state).phase,
        "countdown",
        "{}; motion={}",
        retraction_gate(&f.state).message,
        motion_view(&f.state, &f.state.plumbing_drive.inner.lock().unwrap())
    );
    let packet = Packet::new(
        crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
        &[crate::telemetry_schema::endpoint("GROUND_STATION")],
        "GS",
        f.host.network_time_ms().unwrap(),
        Arc::from([1u8]),
    )
    .unwrap();
    let before = f.host.queue_counters()[2];
    let revision = crate::board_abort::capture_launch_revision_named(&f.state, "Launch").unwrap();
    // Requiredmask0 isolates this final gate from independent required-board
    // qualification. All normal real callers retain their computed masks.
    assert!(
        f.state
            .board_abort
            .admit_launch(&f.state, 0, revision, || f.host.tx_queue(packet.clone()))
            .is_err()
    );
    assert_eq!(f.host.queue_counters()[2], before);
    proof[48..52].copy_from_slice(&0u32.to_le_bytes());
    f.motion = Some(proof);
    f.clock.fetch_add(1_100, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(1_010)).await;
    request_motion_query(&f.state).unwrap();
    f.pump();
    // Refresh actual safe-state proof independently; packet ACK is insufficient.
    assert_eq!(
        f.http(
            "POST",
            "/api/board-recovery/query",
            Some("viewer"),
            json!({"board":"AB"})
        )
        .await
        .status(),
        StatusCode::OK
    );
    f.pump();
    let gate = retraction_gate(&f.state);
    assert!(gate.allowed, "{}", gate.message);
    let before = f.host.queue_counters()[2];
    assert!(
        f.state
            .board_abort
            .admit_launch(&f.state, 0, old_revision, || f
                .host
                .tx_queue(packet.clone()))
            .is_err()
    );
    assert_eq!(f.host.queue_counters()[2], before);
    let fresh = crate::board_abort::capture_launch_revision_named(&f.state, "Launch").unwrap();
    f.state
        .board_abort
        .admit_launch(&f.state, 0, fresh, || f.host.tx_queue(packet.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(f.host.queue_counters()[2], before + 1);
    f.state.manual_abort_intent.request_untimed();
    assert!(
        !retraction_gate(&f.state).allowed,
        "a later Stop cannot reuse completed motion proof"
    );
}

struct MotionCutGuard;
impl MotionCutGuard {
    fn install(path: &Path, stage: u8) -> Self {
        *MOTION_CUT.get_or_init(Default::default).lock().unwrap() = Some((path.into(), stage));
        Self
    }
}
impl Drop for MotionCutGuard {
    fn drop(&mut self) {
        *MOTION_CUT.get().unwrap().lock().unwrap() = None;
    }
}
impl Fixture {
    fn history_reset_body(&self) -> Value {
        let fault = self.state.plumbing_drive.motion_journal.fault_context();
        json!({"backend_instance":format!("{:016x}",self.state.plumbing_drive.issuer),
            "reset_epoch":format!("{:016x}",crate::gse::automatic_generation(&self.state)),
            "fault_fingerprint":fault.fingerprint,"fault_revision":fault.revision,
            "confirmation":"RESET RETRACTION JOURNAL"})
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_history_actual_authorized_reset_is_evidence_preserving_one_use_and_always_closed() {
    let _serial = SERIAL.lock().await;
    let f = Fixture::new_with_history(Some(b"preserved-corrupt-retraction-history\n")).await;
    let path = f.root.join("motion.json");
    let original = fs::read(&path).unwrap();
    let body = f.history_reset_body();
    for (token, wanted) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("viewer"), StatusCode::FORBIDDEN),
        (Some("limited"), StatusCode::FORBIDDEN),
    ] {
        assert_eq!(
            f.http(
                "POST",
                "/api/actuator/plumbing/retraction-history/reset",
                token,
                body.clone()
            )
            .await
            .status(),
            wanted
        );
        assert_eq!(fs::read(&path).unwrap(), original);
    }
    let mut stale = body.clone();
    stale["fault_revision"] = json!(999);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/retraction-history/reset",
            Some("operator"),
            stale
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/retraction-history/reset",
            Some("operator"),
            body.clone()
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(motion_journal::load(&path).unwrap(), None);
    assert!(f.state.plumbing_drive.motion_journal.sealed());
    let gate = retraction_gate(&f.state);
    assert!(gate.configured);
    assert!(!gate.allowed);
    assert_eq!(gate.phase, "needs_check");
    let reload = motion_journal::Journal::new(path);
    assert!(reload.sealed());
    assert_eq!(reload.snapshot(), (None, None));
    let archives: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".retraction-evidence-")
        })
        .collect();
    assert_eq!(archives.len(), 1);
    assert_eq!(fs::read(archives[0].path()).unwrap(), original);
    assert_eq!(
        f.http(
            "POST",
            "/api/actuator/plumbing/retraction-history/reset",
            Some("operator"),
            body
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert!(f.retractions().is_empty());
    assert!(f.sets().is_empty());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_history_each_durable_prefix_reloads_old_fault_or_sealed_closed_never_absent() {
    let _serial = SERIAL.lock().await;
    for stage in 0..=6 {
        let f = Fixture::new_with_history(Some(b"preserved-corrupt-retraction-history\n")).await;
        let path = f.root.join("motion.json");
        let original = fs::read(&path).unwrap();
        let body = f.history_reset_body();
        let cut = MotionCutGuard::install(&path, stage);
        let response = f
            .http(
                "POST",
                "/api/actuator/plumbing/retraction-history/reset",
                Some("operator"),
                body,
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::INSUFFICIENT_STORAGE,
            "prefix{stage}"
        );
        drop(cut);
        assert!(
            path.exists(),
            "readable original cannot disappear at prefix{stage}"
        );
        let reload = motion_journal::Journal::new(path.clone());
        assert!(reload.sealed(), "prefix{stage}");
        let (record, error) = reload.snapshot();
        assert!(record.is_none());
        if error.is_some() {
            assert_eq!(fs::read(&path).unwrap(), original);
        } else {
            assert_eq!(motion_journal::load(&path).unwrap(), None);
        }
        assert!(!retraction_gate(&f.state).allowed);
        assert!(f.retractions().is_empty());
        assert!(f.sets().is_empty());
        assert_eq!(
            f.state.operator_dispatch_inflight.load(Ordering::Acquire),
            0
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_history_cancelled_http_keeps_owned_writer_and_stop_prevents_stale_publication() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new_with_history(Some(b"preserved-corrupt-retraction-history\n")).await;
    let path = f.root.join("motion.json");
    let original = fs::read(&path).unwrap();
    let body = f.history_reset_body();
    let pause = Pause::install(&path, false, true, false);
    let state = f.state.clone();
    let request = tokio::spawn(async move {
        Fixture::request(
            state,
            "POST",
            "/api/actuator/plumbing/retraction-history/reset",
            Some("operator"),
            body,
        )
        .await
    });
    pause.entered().await;
    request.abort();
    let _ = request.await;
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "archive is durable before replacement; original remains in place"
    );
    assert_eq!(
        f.state.operator_dispatch_inflight.load(Ordering::Acquire),
        1
    );
    assert!(f.state.gse.try_lock().is_ok());
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            f.http("POST", "/api/command", Some("operator"), json!("Abort"))
        )
        .await
        .unwrap()
        .status(),
        StatusCode::ACCEPTED
    );
    pause.release();
    f.wait_motion_worker().await;
    assert_eq!(
        f.state.operator_dispatch_inflight.load(Ordering::Acquire),
        0
    );
    assert_eq!(motion_journal::load(&path).unwrap(), None);
    assert!(motion_journal::Journal::new(path).sealed());
    assert!(
        f.state.plumbing_drive.motion_journal.snapshot().1.is_some(),
        "new Stop forbids stale success publication"
    );
    assert!(!retraction_gate(&f.state).allowed);
    assert!(f.retractions().is_empty());
    assert!(f.sets().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public438_actual_prepared_control_retract_and_launch_refuse_held_core_without_delaying_stop()
 {
    let _serial = SERIAL.lock().await;
    for case in ["control", "retract", "launch"] {
        let mut f = Fixture::new().await;
        if case == "retract" {
            f.prime_motion_identity().await;
        }
        for board in crate::board_abort::BOARDS {
            let at = Instant::now();
            f.state
                .board_abort
                .observe_status(board, 42, 8, 1, Some(1), false, true, at, 0, at, 1);
        }
        let original = crate::gse::plumbing_admission_ticket(&f.state);
        let revision =
            crate::board_abort::capture_launch_revision_named(&f.state, "Launch").unwrap();
        let seed = Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            100,
            Arc::from([0u8]),
        )
        .unwrap();
        let packet = Packet::new(
            crate::telemetry_schema::data_type("FLIGHT_COMMAND"),
            &[crate::telemetry_schema::endpoint("FLIGHT_CONTROLLER")],
            "GS",
            100,
            Arc::from([1u8]),
        )
        .unwrap();
        let before = f.host.queue_counters()[2];
        let pause = crate::gse::nonwaiting_admission_tests::Pause::install(&f.state);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = if case == "retract" {
            let origin =
                capture_motion_origin(&f.state, &crate::types::TelemetryCommand::RetractPlumbing)
                    .unwrap();
            crate::gse::queue_plumbing_with_stop_fence(&f.state, &f.host, Some(origin)).unwrap();
            None
        } else {
            let state = f.state.clone();
            let router = f.host.clone();
            Some(std::thread::spawn(move || {
                let result = if case == "control" {
                    crate::gse::queue_controller_with_launch_revision(
                        &state,
                        &router,
                        crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
                        &[crate::rocket_commands::ActuatorBoardCommands::NitrogenOpen as u8],
                        None,
                    )
                    .map(|accepted| format!("accepted={accepted}"))
                    .map_err(|e| e.to_string())
                } else {
                    crate::gse::queue_prepared_launch_packets(
                        &state,
                        &router,
                        crate::gse::nonwaiting_admission_tests::launch_ticket(&state, revision),
                        vec![packet],
                        None,
                    )
                    .map(|n| format!("admitted={n}"))
                };
                result_tx.send(result).unwrap();
            }))
        };
        pause
            .prepared
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("real packet preparation completed outside final gates");
        let router = f.host.clone();
        let holder = std::thread::Builder::new()
            .name("production-sdk-core-holder".into())
            .spawn(move || router.seed_managed_variable(seed))
            .unwrap();
        pause
            .core_entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("actual public SDK core held");
        pause.proceed.send(()).unwrap();
        // Completion while the core is STILL held proves this is the real
        // nonwaiting enqueue path, rather than a Stop that just ran first.
        let attempt = if case == "retract" {
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                while f.state.plumbing_drive.motion_journal.pending() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual motion worker refuses busy core before holder release");
            "owned worker terminal".to_owned()
        } else {
            format!(
                "{:?}",
                result_rx
                    .recv_timeout(std::time::Duration::from_millis(500))
                    .expect("actual final enqueue refuses held core")
            )
        };
        let stop = f.state.manual_abort_intent.clone();
        let (accepted, received) = std::sync::mpsc::channel();
        let requester = std::thread::spawn(move || {
            accepted.send(stop.request_untimed()).unwrap();
        });
        let epoch = received
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("manual Stop retained before SDK core release");
        assert_eq!(tokio::time::timeout(std::time::Duration::from_millis(200),
            f.http("POST","/api/command",Some("operator"),json!("Abort"))).await
            .expect("actual HTTP manual Stop stays prompt while SDK core is held").status(),StatusCode::ACCEPTED);
        pause.release();
        holder.join().unwrap().unwrap();
        requester.join().unwrap();
        if let Some(worker) = worker {
            worker.join().unwrap();
        }
        assert_eq!(
            f.host.queue_counters()[2],
            before,
            "{case}: no SDK ownership on core refusal"
        );
        let retry = Packet::new(
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),
            &[crate::telemetry_schema::endpoint("ACTUATOR_BOARD")],
            "GS",
            100,
            Arc::from([1u8]),
        )
        .unwrap();
        assert!(
            crate::gse::with_plumbing_admission(&f.state, original, || f.host.try_tx_queue(retry))
                .is_err(),
            "the original ticket cannot silently retry after Stop"
        );
        assert_eq!(f.host.queue_counters()[2], before);
        println!(
            "PUBLIC438_HELD_CORE case={case} prepared_before_hold=true attempt={attempt} prompt_stop_epoch={epoch} SDK_owners=0 stale_retry_refused=true"
        );
        drop(pause);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn motion_post_admission_checks_are_two_bounded_queries_and_cancel_before_each_without_sleeping_lease()
 {
    let _serial = SERIAL.lock().await;
    for cancel_after in [Some(0), Some(1), None] {
        let mut f = Fixture::new().await;
        f.prime_motion_identity().await;
        let origin =
            capture_motion_origin(&f.state, &crate::types::TelemetryCommand::RetractPlumbing)
                .unwrap();
        crate::gse::queue_plumbing_with_stop_fence(&f.state, &f.host, Some(origin)).unwrap();
        f.wait_motion_worker().await;
        let record = f.state.plumbing_drive.motion_journal.snapshot().0.unwrap();
        let count = |f: &Fixture| {
            f.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|packet| {
                    packet.data_type() == crate::telemetry_schema::data_type(REQUEST_TYPE)
                        && packet.payload().first() == Some(&2)
                })
                .count()
        };
        let before = count(&f);
        // Neither duplicate observer work nor repeat scheduling creates a poller.
        schedule_post_motion_queries(f.state.clone(), record.clone());
        schedule_post_motion_queries(f.state.clone(), record);
        assert_eq!(
            f.state.operator_dispatch_inflight.load(Ordering::Acquire),
            0,
            "no lease across timer sleep"
        );
        if cancel_after == Some(0) {
            f.state.manual_abort_intent.request_untimed();
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        f.pump();
        let first = count(&f) - before;
        assert_eq!(first, if cancel_after == Some(0) { 0 } else { 1 });
        assert_eq!(
            f.state.operator_dispatch_inflight.load(Ordering::Acquire),
            0
        );
        if cancel_after == Some(1) {
            f.state.manual_abort_intent.request_untimed();
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        f.pump();
        let total = count(&f) - before;
        assert_eq!(
            total,
            match cancel_after {
                Some(0) => 0,
                Some(1) => 1,
                None => 2,
                _ => unreachable!(),
            }
        );
        tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
        f.pump();
        assert_eq!(count(&f) - before, total, "no recurring/reconnect polling");
        assert_eq!(
            f.retractions().len(),
            1,
            "read-only checks never retry motion"
        );
    }
}

#[cfg(feature = "testing")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modeled_retraction_actual_acceptance_uses_monotonic45_and_original_stop_context_not_utc_age()
 {
    if std::env::var_os("MODEL_RETRACTION_CHILD").is_none(){
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","actuator_plumbing::http_tests::modeled_retraction_actual_acceptance_uses_monotonic45_and_original_stop_context_not_utc_age","--nocapture"]).env("MODEL_RETRACTION_CHILD","1").env("GS_SIM_MODE","1").env("GS_RRLS_ROLE","primary").output().unwrap();
        assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
    }
    let _serial = SERIAL.lock().await;
    let f = Fixture::new().await;
    #[cfg(not(any(feature="hitl_mode",feature="test_fire_mode")))]
    {
        crate::flight_sim::initialize_model_owners(&f.state,0,0);
        let rocket=crate::comms::DummyComms::new("Rocket Comms");let fill=crate::comms::DummyComms::new("Umbilical Comms");
        crate::testing_board_lifecycle::bind_selected(crate::comms::CommsDevice::testing_model_link(&rocket),crate::comms::CommsDevice::testing_model_link(&fill));
    }
    crate::flight_sim::reset_simulation();
    let origin =
        capture_motion_origin(&f.state, &crate::types::TelemetryCommand::RetractPlumbing).unwrap();
    let command = crate::types::TelemetryCommand::RetractPlumbing;
    crate::gse::admit_simulated_retraction_stamped(&f.state, Some(origin), |source,now| {
        crate::flight_sim::handle_command_stamped(&command,source,now)
    })
    .unwrap();
    let start = f
        .state
        .plumbing_drive
        .inner
        .lock()
        .unwrap()
        .modeled_retraction
        .unwrap()
        .started;
    let gate = retraction_gate(&f.state);
    assert!(gate.configured);
    assert!(!gate.allowed);
    assert_eq!(gate.phase, "countdown");
    assert_eq!(
        modeled_remaining(start, start + std::time::Duration::from_millis(44_999)),
        1
    );
    assert_eq!(
        modeled_remaining(start, start + std::time::Duration::from_millis(45_000)),
        0
    );
    crate::gse::admit_simulated_retraction_stamped(&f.state, Some(origin), |source,now| {
        crate::flight_sim::handle_command_stamped(&command,source,now)
    })
    .unwrap();
    assert_eq!(
        f.state
            .plumbing_drive
            .inner
            .lock()
            .unwrap()
            .modeled_retraction
            .unwrap()
            .started,
        start,
        "duplicate original intent cannot restart the modeled anchor"
    );
    f.clock.fetch_add(60_000, Ordering::Relaxed);
    assert!(!retraction_gate(&f.state).allowed,"a network UTC jump is not elapsed physical/model time");
    assert!(crate::gse::admit_simulated_retraction_stamped(&f.state,Some(origin),|source,now|crate::flight_sim::handle_command_stamped(&command,source,now)).is_err(),"expired original command cannot produce another effect");
    assert_eq!(f.state.plumbing_drive.inner.lock().unwrap().modeled_retraction.unwrap().started,start,"expiry cannot refresh the accepted monotonic anchor");
    // Explicit test-only monotonic elapsed boundary; no hardware completion or
    // wall-clock authority is fabricated by the production model.
    f.state
        .plumbing_drive
        .inner
        .lock()
        .unwrap()
        .modeled_retraction
        .as_mut()
        .unwrap()
        .started = Instant::now() - std::time::Duration::from_secs(45);
    let gate = retraction_gate(&f.state);
    assert!(gate.allowed);
    assert!(gate.message.contains("Testing model"));
    f.state.manual_abort_intent.request_untimed();
    assert!(
        !retraction_gate(&f.state).allowed,
        "a later Stop invalidates the original modeled operation"
    );
    assert!(
        crate::gse::admit_simulated_retraction_stamped(&f.state, Some(origin), |source,now| {
            crate::flight_sim::handle_command_stamped(&command,source,now)
        })
        .is_err()
    );
    assert!(
        f.retractions().is_empty(),
        "pure model did not acquire physical SDK motion ownership"
    );
    crate::flight_sim::reset_simulation();
}

#[cfg(feature = "hitl_mode")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hitl_safety_interlocks_actual_http_receiver_changes_require_permission_and_invalidate_old_launch_origin()
 {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    *f.state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    assert!(crate::auto_zero::before_fill(&f.state, &crate::types::TelemetryCommand::StartFill));
    assert!(f.state.auto_zero.lock().unwrap().pending());
    f.state.gse.lock().unwrap().engine.status.phase = gse_sequence::Phase::Paused;
    let capture = f.state.loadcell_processing.start("KG1000", 60, true).unwrap();
    let original_generation = crate::gse::automatic_generation(&f.state);
    f.set_host_worker_running(true);
    let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
        f.state.clone(),
        f.host.clone(),
        Vec::new(),
        f.cmd_rx.take().unwrap(),
        f.db_rx.take().unwrap(),
        f.state.shutdown_subscribe(),
    ));
    for (index, name, initial) in [
        (0, "TogglePlumbingDelayInterlock", false),
        (1, "ToggleChecklistInterlock", true),
        (2, "ToggleSupplyInterlock", true),
        (3,"ToggleBoardStateProofInterlock",false),
    ] {
        assert_eq!(f.state.hitl_safety_interlock_enabled(index), initial);
        let revision =
            crate::board_abort::capture_launch_revision_named(&f.state, "Launch").unwrap();
        for (token, wanted) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("viewer"), StatusCode::FORBIDDEN),
            (Some("limited"), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                f.http("POST", "/api/command", token, json!(name))
                    .await
                    .status(),
                wanted
            );
            assert_eq!(f.state.hitl_safety_interlock_enabled(index), initial);
        }
        assert_eq!(
            f.http("POST", "/api/command", Some("operator"), json!(name))
                .await
                .status(),
            StatusCode::OK
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while f
                .state
                .action_policy_snapshot()
                .controls
                .iter()
                .find(|control| control.cmd == name)
                .is_none_or(|control| control.actuated != Some(!initial))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            f.state.board_abort.abort_revision() > revision,
            "changed policy cannot refresh an old queued Launch"
        );
        let policy = f.state.action_policy_snapshot();
        let control = policy
            .controls
            .iter()
            .find(|control| control.cmd == name)
            .unwrap();
        assert_eq!(control.actuated, Some(!initial));
        assert!(f.state.auto_zero.lock().unwrap().pending(), "policy toggle did not cancel pending fill zero");
        assert_eq!(f.state.gse.lock().unwrap().engine.status.phase, gse_sequence::Phase::Paused,
            "policy toggle neither cancels nor resumes the filled sequence");
        let current_capture = f.state.loadcell_processing.status();
        assert!(current_capture.running);
        assert_eq!(current_capture.session_id, capture.session_id);
        assert_eq!(crate::gse::automatic_generation(&f.state), original_generation);
        assert_eq!(
            f.state.checklist_enforcement_enabled(),
            f.state.hitl_safety_interlock_enabled(1)
        );
        assert_eq!(
            f.state.supply_enforcement_enabled(),
            f.state.hitl_safety_interlock_enabled(2)
        );
    }
    assert!(
        f.retractions().is_empty(),
        "toggle did not initiate motion or Launch"
    );
    f.state.loadcell_processing.stop(&capture.session_id).unwrap();
    f.state.request_shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap();
    f.set_host_worker_running(false);
}

#[tokio::test(flavor="multi_thread",worker_threads=2)]
async fn public438_temporary_core_busy_retries_identical_unowned_control_once_and_stop_during_retry_refuses(){
    let _serial=SERIAL.lock().await;
    for stop_before_release in [false,true] {
        let mut f=Fixture::new().await;
        let pause=crate::gse::nonwaiting_admission_tests::Pause::install(&f.state);
        let state=f.state.clone();let router=f.host.clone();
        let worker=std::thread::spawn(move||crate::gse::queue_controller_with_launch_revision(&state,&router,
            crate::telemetry_schema::data_type("ACTUATOR_COMMAND"),&[crate::rocket_commands::ActuatorBoardCommands::NitrogenOpen as u8],None));
        pause.prepared.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let original=pause.packet.lock().unwrap().clone().unwrap();
        let seed=Packet::new(crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],"GS",100,Arc::from([0u8])).unwrap();
        let router=f.host.clone();let holder=std::thread::Builder::new().name("production-sdk-core-holder".into()).spawn(move||router.seed_managed_variable(seed)).unwrap();
        pause.core_entered.recv_timeout(std::time::Duration::from_secs(2)).unwrap();pause.proceed.send(()).unwrap();
        pause.busy.recv_timeout(std::time::Duration::from_millis(100)).expect("actual original first try refused with no SDK owner");
        if stop_before_release {assert_eq!(tokio::time::timeout(std::time::Duration::from_millis(100),f.http("POST","/api/command",Some("operator"),json!("Abort"))).await.unwrap().status(),StatusCode::ACCEPTED);}
        pause.release();holder.join().unwrap().unwrap();
        assert_eq!(worker.join().unwrap().unwrap(),!stop_before_release);
        f.pump();
        let controls:Vec<_>=f.requests.lock().unwrap().iter().filter(|p|p.data_type()==original.data_type()&&p.payload()==original.payload()).cloned().collect();
        if stop_before_release {assert!(controls.is_empty(),"new Stop invalidates original retry before any actual owner");}
        else {assert_eq!(controls.len(),1);assert_eq!(controls[0].packet_id(),original.packet_id());assert_eq!(controls[0].timestamp(),original.timestamp());}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicitly_expired_motion_during_owned_journal_never_gets_new_timestamp_or_sdk_admission() {
    let _serial = SERIAL.lock().await;
    let mut f = Fixture::new().await;
    f.prime().await;
    assert_eq!(f.http("POST", "/api/actuator/plumbing/settings", Some("operator"), f.body("auxiliary", true)).await.status(), StatusCode::ACCEPTED);
    f.pump();
    f.prime_motion_identity().await;
    let origin = capture_motion_origin(&f.state, &crate::types::TelemetryCommand::RetractPlumbing).unwrap();
    validate_motion_origin(&f.state, Some(origin)).unwrap();
    let path = f.state.plumbing_drive.motion_journal.path().to_owned();
    let pause = Pause::install(&path, false, true, false);
    crate::gse::queue_plumbing_with_stop_fence(&f.state, &f.host, Some(origin)).unwrap();
    pause.entered().await;
    // Intentional elapsed time still expires the original operation. The
    // fixture correction changes only implicit progress from CPU polling.
    f.clock.fetch_add(6_000, Ordering::Relaxed);
    assert!(validate_motion_origin(&f.state, Some(origin)).is_err());
    pause.release();
    f.wait_motion_worker().await;
    assert!(f.retractions().is_empty());
    assert_eq!(motion_journal::load(&path).unwrap().unwrap().original_utc_ms, origin.source_ms.unwrap());
    assert_eq!(f.state.plumbing_drive.inner.lock().unwrap().last_motion_stamp, 0);
}
