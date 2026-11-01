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
    #[cfg(not(feature = "hitl_mode"))]
    pub(crate) cmd_rx: Option<tokio::sync::mpsc::Receiver<crate::types::TelemetryCommand>>,
    #[cfg(not(feature = "hitl_mode"))]
    db_rx: Option<tokio::sync::mpsc::Receiver<crate::DbQueueItem>>,
    clock: Arc<AtomicU64>,
    host_side: usize,
    board_side: usize,
    outgoing: Arc<Mutex<Vec<Vec<u8>>>>,
    incoming: Arc<Mutex<Vec<Vec<u8>>>>,
    requests: Arc<Mutex<Vec<Packet>>>,
    handled: usize,
    host_worker_running: bool,
    mode: u8,
    result: u8,
    outputs: u32,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.request_shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    pub(crate) async fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("plumbing-http-{:016x}", rand::random::<u64>()));
        fs::create_dir(&root).unwrap();
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
            Arc::get_mut(&mut s.plumbing_drive).unwrap().path = root.join("drive.json");
        }
        state.set_local_flight_state(FlightState::Idle);
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        for (user, commands) in [("viewer", 0), ("operator", 1), ("limited", 1)] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,?,0,9999999999999)").bind(user).bind(user).bind(commands).execute(&state.auth_db).await.unwrap();
        }
        let clock = Arc::new(AtomicU64::new(0));
        let tick = clock.clone();
        let weak = Arc::downgrade(&state);
        let host = Arc::new(Router::new_with_clock(
            RouterConfig::new([EndpointHandler::new_packet_handler(
                crate::telemetry_schema::endpoint("GROUND_STATION"),
                move |packet| {
                    let state = weak.upgrade().unwrap();
                    receive_status(&state, packet);
                    crate::board_recovery::receive_status(&state, packet);
                    if packet.data_type() == crate::telemetry_schema::data_type("UMBILICAL_STATUS")
                    {
                        crate::telemetry_task::apply_umbilical_status_at_ingress(&state, packet);
                    }
                    Ok(())
                },
            )])
            .with_sender("GS"),
            Box::new(move || tick.load(Ordering::Relaxed)),
        ));
        host.try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_563_200_000,
        ))
        .unwrap();
        assert!(state.topology_router.set(host.clone()).is_ok());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let tick = clock.clone();
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
            Box::new(move || tick.load(Ordering::Relaxed)),
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
            #[cfg(not(feature = "hitl_mode"))]
            cmd_rx: Some(_cmd_rx),
            #[cfg(not(feature = "hitl_mode"))]
            db_rx: Some(_db_rx),
            clock,
            host_side,
            board_side,
            outgoing,
            incoming,
            requests,
            handled: 0,
            host_worker_running: false,
            mode: 0,
            result: 0,
            outputs: 16,
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
    pub(crate) fn pump(&mut self) {
        for _ in 0..80 {
            self.clock.fetch_add(1, Ordering::Relaxed);
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
    f.host_worker_running = true;
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
    // Immediate new-mode intent must not need a dedup delay.
    let stamp = f.host.network_time_ms().unwrap();
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
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
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
    .await
    .unwrap();
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
    f.host_worker_running = false;
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
    assert_eq!(
        f.http("POST", "/api/command", Some("operator"), json!("Abort"))
            .await
            .status(),
        StatusCode::ACCEPTED
    );

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
