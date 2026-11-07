//! Actual HTTP -> original admission -> production DB actor -> GSE/public SDK.
//! Only endpoints are disposable memory peers; no GPIO device or radio is opened.
use super::*;
use crate::{
    telemetry_db::RecordingModeWire,
    telemetry_task::recording_owner::{TestOwner, test_hooks},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

type Sent = Arc<Mutex<Vec<(String, Vec<u8>)>>>;
type Observed = Arc<Mutex<Vec<(RecordingModeWire, u64)>>>;
pub(crate) struct Fixture {
    _owner: TestOwner,
    _commands: tokio::sync::mpsc::Receiver<crate::types::TelemetryCommand>,
    root: crate::recording_export::tests::TestDirectory,
    pub(crate) state: Arc<AppState>,
    pub(crate) sent: Sent,
    peer: Arc<sedsnet::router::Router>,
    observed: Observed,
}
impl Fixture {
    async fn new(full: bool) -> Self {
        Self::configured(full, gse_sequence::Phase::Paused, false).await
    }
    pub(crate) async fn new_phase(full: bool, phase: gse_sequence::Phase) -> Self {
        Self::configured(full, phase, false).await
    }
    async fn configured(full: bool, phase: gse_sequence::Phase, blocked: bool) -> Self {
        let root = crate::recording_export::tests::TestDirectory::new();
        let weak = Arc::new(Mutex::new(std::sync::Weak::<AppState>::new()));
        let binding = weak.clone();
        let observed: Observed = Default::default();
        let seen = observed.clone();
        let observer = Arc::new(move |_: &sedsnet::packet::Packet| {
            let Some(state) = binding.lock().unwrap().upgrade() else {
                return;
            };
            seen.lock().unwrap().push((
                state.recording_status_snapshot().mode,
                state.fill_recording.ready_instance(),
            ));
        });
        let (mut state, sent, peer) =
            crate::gse::fill_recording_test_state(0., phase, Some(observer)).await;
        let target = state.fill_targets_snapshot().nitrous.target_mass_kg;
        state.gse.lock().unwrap().observe_mass(Some(if full {
            target
        } else {
            target - target.signum()
        }));
        let users = root.0.join("users.json");
        std::fs::write(&users,serde_json::to_vec(&serde_json::json!({"version":1,"users":[{"username":"fill-operator","password":{"salt_b64":"","hash_b64":""},"roles":[],"permissions":{"view_data":true,"send_commands":true},"command_access":{"allowed_commands":["StartFill","AutoColdFlow"]}}]})).unwrap()).unwrap();
        let (commands, receive_commands) = tokio::sync::mpsc::channel(4);
        Arc::get_mut(&mut state).unwrap().cmd_tx = commands;
        Arc::get_mut(&mut state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(users));
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('fill-token','fill-operator','session',1,1,0,9999999999999)").execute(&state.auth_db).await.unwrap();
        if blocked {
            std::fs::write(root.0.join("recordings"), b"owned blocker").unwrap();
        }
        let owner = TestOwner::start(&mut state, &root.0.join("recordings"));
        *weak.lock().unwrap() = Arc::downgrade(&state);
        Self {
            _owner: owner,
            _commands: receive_commands,
            root,
            state,
            sent,
            peer,
            observed,
        }
    }
    pub(crate) fn files(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.root.0.join("recordings"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("groundstation_recording_")
                    && p.extension().is_some_and(|e| e == "db")
            })
            .collect()
    }
    pub(crate) fn pump(&self) {
        crate::gse::pump_fill_recording_test_link(&self.state, &self.peer);
    }
}
async fn press(state: Arc<AppState>, token: Option<&str>) -> (StatusCode, String) {
    let app = crate::web::router(state, "unused-fixture-video-password".into());
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/command")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::from("\"StartFill\"")).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}
async fn held(arrived: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || arrived.recv_timeout(Duration::from_secs(2)).unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_http_fill_has_recording_ready_before_public_sdk_output() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_fill_has_recording_ready_before_public_sdk_output",
    ) {
        return;
    }
    let f = Fixture::new(false).await;
    let result = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    f.pump();
    assert!(!f.sent.lock().unwrap().is_empty());
    let observations = f.observed.lock().unwrap();
    assert!(!observations.is_empty());
    assert!(
        observations
            .iter()
            .all(|(mode, instance)| *mode == RecordingModeWire::Recording && *instance != 0)
    );
    drop(observations);
    assert_eq!(f.files().len(), 1);
    assert_eq!(
        f.state.recording_status_snapshot().mode,
        RecordingModeWire::Recording
    );
}
#[tokio::test]
async fn actual_http_full_noop_logs_without_run_tare_data_reset_or_valve_effects() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_full_noop_logs_without_run_tare_data_reset_or_valve_effects",
    ) {
        return;
    }
    let f = Fixture::new(true).await;
    let before = crate::gse::fill_recording_progress_snapshot(&f.state);
    let zero = f
        .state
        .auto_zero
        .lock()
        .unwrap()
        .capture_identity_for_test();
    let calibration = serde_json::to_value(&*f.state.loadcell_calibration.lock().unwrap()).unwrap();
    let mut reset = f.state.dashboard_reset_tx.subscribe();
    let result = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
    assert_eq!(f.files().len(), 1);
    assert_eq!(
        crate::gse::fill_recording_progress_snapshot(&f.state),
        before
    );
    assert_eq!(
        f.state
            .auto_zero
            .lock()
            .unwrap()
            .capture_identity_for_test(),
        zero
    );
    assert_eq!(
        serde_json::to_value(&*f.state.loadcell_calibration.lock().unwrap()).unwrap(),
        calibration
    );
    assert!(matches!(
        reset.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let path = f.state.recording_status_snapshot().db_path.unwrap();
    let second = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(
        f.state.recording_status_snapshot().db_path.as_deref(),
        Some(path.as_str())
    );
    assert_eq!(f.files().len(), 1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}
#[tokio::test]
async fn actual_http_invalid_fill_limits_preserve_mode_policy_and_have_no_effects() {
    if crate::operator_queue::isolated_case(module_path!(),
        "actual_http_invalid_fill_limits_preserve_mode_policy_and_have_no_effects") {
        return;
    }
    let f = Fixture::new_phase(false, gse_sequence::Phase::Passed).await;
    f.state.gse.lock().unwrap().engine.config.maximum_zero_offset_psi = None;
    let progress = crate::gse::fill_recording_progress_snapshot(&f.state);
    let zero = f.state.auto_zero.lock().unwrap().capture_identity_for_test();
    let calibration = serde_json::to_value(&*f.state.loadcell_calibration.lock().unwrap()).unwrap();
    let generation = crate::gse::automatic_generation(&f.state);
    let result = press(f.state.clone(), Some("fill-token")).await;
    #[cfg(feature = "hitl_mode")]
    {
        assert_eq!(result.0, StatusCode::CONFLICT, "{}", result.1);
        let error: serde_json::Value = serde_json::from_str(&result.1).unwrap();
        assert_eq!(error["message"], "Set the maximum acceptable empty-tank PT offset");
        assert_eq!(error["reason"]["validation"], "Set the maximum acceptable empty-tank PT offset");
        assert!(!result.1.contains("expired"));
    }
    #[cfg(not(feature = "hitl_mode"))]
    {
        // The ordinary action-policy gate rejects this invalid setup first.
        assert_eq!(result.0, StatusCode::FORBIDDEN, "{}", result.1);
        assert_eq!(result.1, "command disabled");
    }
    f.pump();
    assert!(f.files().is_empty());
    assert!(f.sent.lock().unwrap().is_empty());
    assert!(f.observed.lock().unwrap().is_empty());
    assert_eq!(f.state.fill_recording.ready_instance(), 0);
    assert_eq!(f.state.recording_status_snapshot().mode, RecordingModeWire::Idle);
    assert_eq!(crate::gse::fill_recording_progress_snapshot(&f.state), progress);
    assert_eq!(f.state.auto_zero.lock().unwrap().capture_identity_for_test(), zero);
    assert_eq!(serde_json::to_value(&*f.state.loadcell_calibration.lock().unwrap()).unwrap(), calibration);
    assert_eq!(crate::gse::automatic_generation(&f.state), generation);
}

#[tokio::test]
async fn actual_http_denied_and_stop_during_open_never_publish_recording_or_outputs() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_denied_and_stop_during_open_never_publish_recording_or_outputs",
    ) {
        return;
    }
    let f = Fixture::new(false).await;
    let denied = press(f.state.clone(), Some("invalid-token")).await;
    assert!(matches!(
        denied.0,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
    assert!(f.files().is_empty());
    let (arrived, release) = test_hooks::hold_after_open(&f.state);
    let state = f.state.clone();
    let pending = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    f.state.manual_abort_intent.request();
    release.send(()).unwrap();
    let result = pending.await.unwrap();
    assert!(!result.0.is_success(), "{}", result.1);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&result.1).unwrap()["reason"], "stale");
    f.pump();
    assert!(f.files().is_empty());
    assert_eq!(f.state.fill_recording.ready_instance(), 0);
    assert_eq!(
        f.state.recording_status_snapshot().mode,
        RecordingModeWire::Idle
    );
    assert!(f.sent.lock().unwrap().is_empty());
    assert!(f.state.manual_abort_intent.pending_epoch().is_some());
}
#[tokio::test]
async fn actual_http_permission_loss_after_open_is_checked_and_retry_is_not_dedup_deadend() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_permission_loss_after_open_is_checked_and_retry_is_not_dedup_deadend",
    ) {
        return;
    }
    let f = Fixture::new(true).await;
    let (arrived, release) = test_hooks::hold_after_open(&f.state);
    let state = f.state.clone();
    let pending = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    sqlx::query("UPDATE auth_sessions SET can_send_commands=0 WHERE token='fill-token'")
        .execute(&f.state.auth_db)
        .await
        .unwrap();
    release.send(()).unwrap();
    let result = pending.await.unwrap();
    assert_eq!(result.0, StatusCode::FORBIDDEN, "{}", result.1);
    assert!(f.files().is_empty());
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
    sqlx::query("UPDATE auth_sessions SET can_send_commands=1 WHERE token='fill-token'")
        .execute(&f.state.auth_db)
        .await
        .unwrap();
    let retry = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
    assert_eq!(f.files().len(), 1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn actual_http_concurrent_pending_press_is_busy_and_keeps_one_session() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_concurrent_pending_press_is_busy_and_keeps_one_session",
    ) {
        return;
    }
    let f = Fixture::new(true).await;
    let (arrived, release) = test_hooks::hold_after_open(&f.state);
    let state = f.state.clone();
    let first = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    assert_eq!(
        f.state.recording_status_snapshot().mode,
        RecordingModeWire::Idle
    );
    let second = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(second.0, StatusCode::SERVICE_UNAVAILABLE, "{}", second.1);
    assert!(second.1.contains("busy"));
    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    assert_eq!(f.files().len(), 1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}
#[tokio::test]
async fn actual_http_cancel_during_open_removes_only_unpublished_candidate() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_cancel_during_open_removes_only_unpublished_candidate",
    ) {
        return;
    }
    let f = Fixture::new(true).await;
    let sentinel = f.root.0.join("recordings/archive.db");
    std::fs::write(&sentinel, b"keep").unwrap();
    let (arrived, release) = test_hooks::hold_after_open(&f.state);
    let state = f.state.clone();
    let pending = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    for _ in 0..100 {
        if f.files().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(f.files().is_empty());
    assert_eq!(std::fs::read(sentinel).unwrap(), b"keep");
    assert_eq!(f.state.fill_recording.ready_instance(), 0);
    assert_eq!(
        f.state.recording_status_snapshot().mode,
        RecordingModeWire::Idle
    );
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}
#[tokio::test]
async fn actual_http_initial_full_noop_never_promotes_to_started_after_recording_wait() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_initial_full_noop_never_promotes_to_started_after_recording_wait",
    ) {
        return;
    }
    let f = Fixture::new(true).await;
    let zero = f
        .state
        .auto_zero
        .lock()
        .unwrap()
        .capture_identity_for_test();
    let (arrived, release) = test_hooks::hold_after_open(&f.state);
    let state = f.state.clone();
    let pending = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    let target = f.state.fill_targets_snapshot().nitrous.target_mass_kg;
    f.state
        .gse
        .lock()
        .unwrap()
        .observe_mass(Some(target - target.signum()));
    release.send(()).unwrap();
    let result = pending.await.unwrap();
    assert_eq!(result.0, StatusCode::CONFLICT, "{}", result.1);
    assert_eq!(f.files().len(), 1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
    assert_eq!(
        f.state
            .auto_zero
            .lock()
            .unwrap()
            .capture_identity_for_test(),
        zero
    );
    assert_eq!(
        f.state.gse.lock().unwrap().engine.status.phase,
        gse_sequence::Phase::Paused
    );
}
#[tokio::test]
async fn actual_http_five_second_pre_auth_deadline_is_not_renewed_after_auth() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_five_second_pre_auth_deadline_is_not_renewed_after_auth",
    ) {
        return;
    }
    let f = Fixture::new(false).await;
    let (arrived, release) = test_hooks::hold_http_auth(&f.state);
    let state = f.state.clone();
    let pending = tokio::spawn(async move { press(state, Some("fill-token")).await });
    held(arrived).await;
    tokio::time::sleep(Duration::from_millis(5100)).await;
    assert!(!pending.is_finished());
    // Fresh SENSOR observations do not renew the old HUMAN admission.
    {
        let target = f.state.fill_targets_snapshot().nitrous.target_mass_kg;
        let mut rt = f.state.gse.lock().unwrap();
        rt.observe_pressure(Some(0.));
        rt.observe_mass(Some(target - target.signum()));
    }
    release.send(()).unwrap();
    let result = pending.await.unwrap();
    assert_eq!(result.0, StatusCode::CONFLICT, "{}", result.1);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&result.1).unwrap()["reason"], "stale");
    assert!(f.files().is_empty());
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}

pub(crate) fn enable_owned_zero(f: &Fixture) {
    let mut targets = f.state.fill_targets_snapshot();
    targets.fill_source = crate::fill_targets::FillSource::Kg1000Absolute;
    f.state.set_fill_targets(targets);
    let mut calibration = crate::loadcell::LoadcellCalibrationFile::default();
    calibration.ch1.m = Some(1.);
    calibration.ch1.b = Some(0.);
    *f.state.loadcell_calibration.lock().unwrap() = calibration;
    *f.state.auto_zero.lock().unwrap() = crate::auto_zero::Runtime::testing(true);
    f.state.gse.lock().unwrap().observe_mass(Some(0.));
}
pub(crate) async fn feed_zero(
    f: &Fixture,
    delay: Duration,
    mut service: impl FnMut(usize),
) -> Duration {
    let began = Instant::now();
    for id in 1..=200 {
        tokio::time::sleep(delay).await;
        service(id);
        f.state.gse.lock().unwrap().observe_pressure(Some(0.));
        f.state
            .auto_zero
            .lock()
            .unwrap()
            .observe_packet("KG1000", 0.2, id as u64);
        f.pump();
    }
    began.elapsed()
}
pub(crate) async fn settle_zero(f: &Fixture, expect_output: bool) {
    for _ in 0..200 {
        f.pump();
        if !f.state.auto_zero.lock().unwrap().pending()
            && (!expect_output || !f.sent.lock().unwrap().is_empty())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f.pump();
}
#[tokio::test]
async fn actual_http_recording_failure_is_typed_without_abort_and_fresh_retry_works() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_http_recording_failure_is_typed_without_abort_and_fresh_retry_works",
    ) {
        return;
    }
    let f = Fixture::configured(true, gse_sequence::Phase::Paused, true).await;
    let before = f.state.local_flight_state_snapshot();
    let result = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(result.0, StatusCode::INSUFFICIENT_STORAGE, "{}", result.1);
    assert!(result.1.contains("storage"));
    assert_eq!(f.state.local_flight_state_snapshot(), before);
    assert!(!f.state.abort_indicator_latched());
    assert_eq!(f.state.fill_recording.ready_instance(), 0);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
    let directory = f.root.0.join("recordings");
    assert_eq!(std::fs::read(&directory).unwrap(), b"owned blocker");
    std::fs::remove_file(&directory).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let retry = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(retry.0, StatusCode::OK, "{}", retry.1);
    assert_eq!(f.files().len(), 1);
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
}
#[tokio::test]
async fn accepted_local_zero_keeps_actual_seven_second_capture_after_initial_five_seconds() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "accepted_local_zero_keeps_actual_seven_second_capture_after_initial_five_seconds",
    ) {
        return;
    }
    let f = Fixture::new_phase(false, gse_sequence::Phase::Passed).await;
    enable_owned_zero(&f);
    let result = press(f.state.clone(), Some("fill-token")).await;
    assert_eq!(result.0, StatusCode::OK, "{}", result.1);
    assert!(f.state.auto_zero.lock().unwrap().pending());
    f.pump();
    assert!(f.sent.lock().unwrap().is_empty());
    let elapsed = feed_zero(&f, Duration::from_millis(25), |_| {}).await;
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(7),
        "{elapsed:?}"
    );
    settle_zero(&f, true).await;
    assert!(!f.state.auto_zero.lock().unwrap().pending());
    assert!(!f.sent.lock().unwrap().is_empty());
    assert_eq!(
        f.state.loadcell_calibration.lock().unwrap().ch1_zero_raw,
        Some(0.2)
    );
    assert!(
        f.observed
            .lock()
            .unwrap()
            .iter()
            .all(|(mode, id)| *mode == RecordingModeWire::Recording && *id != 0)
    );
    assert_eq!(f.files().len(), 1);
}

#[tokio::test]
async fn actual_websocket_fill_is_nonblocking_and_closed_or_stopped_source_never_publishes() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "actual_websocket_fill_is_nonblocking_and_closed_or_stopped_source_never_publishes",
    ) {
        return;
    }
    use futures::{SinkExt, StreamExt};
    for case in ["current", "closed", "stop"] {
        let f = Fixture::new(true).await;
        // This dedicated operator is allowed to retain a genuine emergency Stop.
        let users = f.state.auth.path();
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(users).unwrap()).unwrap();
        config["users"][0]["command_access"]["allowed_commands"] =
            serde_json::json!(["StartFill", "AutoColdFlow", "Abort"]);
        std::fs::write(users, serde_json::to_vec(&config).unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = crate::web::router(f.state.clone(), "fixture".into());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{address}/ws?token=fill-token"))
                .await
                .unwrap();
        let (arrived, release) = test_hooks::hold_after_open(&f.state);
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"cmd":"StartFill"}"#.into(),
            ))
            .await
            .unwrap();
        held(arrived).await;
        assert_eq!(
            f.state.recording_status_snapshot().mode,
            RecordingModeWire::Idle
        );
        if case == "closed" {
            socket.close(None).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                while let Some(message) = socket.next().await {
                    if matches!(
                        message,
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_)
                    ) {
                        break;
                    }
                }
            })
            .await;
            tokio::task::yield_now().await;
        }
        if case == "stop" {
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    r#"{"cmd":"Abort"}"#.into(),
                ))
                .await
                .unwrap();
            for _ in 0..100 {
                if f.state.manual_abort_intent.pending_epoch().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                f.state.manual_abort_intent.pending_epoch().is_some(),
                "disk wait blocked this websocket's manual Stop owner"
            );
        }
        release.send(()).unwrap();
        for _ in 0..100 {
            if case == "current"
                && f.state.recording_status_snapshot().mode == RecordingModeWire::Recording
            {
                break;
            }
            if case != "current" && f.files().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(f.files().len(), usize::from(case == "current"), "{case}");
        f.pump();
        assert!(f.sent.lock().unwrap().is_empty());
        if case != "closed" {
            let _ = socket.close(None).await;
        }
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn accepted_zero_rechecks_permission_session_stop_reset_and_recording_receipt() {
    if crate::operator_queue::isolated_case(
        module_path!(),
        "accepted_zero_rechecks_permission_session_stop_reset_and_recording_receipt",
    ) {
        return;
    }
    for revoke in ["permission", "session", "stop", "reset", "recording"] {
        let f = Fixture::new_phase(false, gse_sequence::Phase::Passed).await;
        enable_owned_zero(&f);
        let result = press(f.state.clone(), Some("fill-token")).await;
        assert_eq!(result.0, StatusCode::OK, "{revoke}: {}", result.1);
        assert!(f.state.auto_zero.lock().unwrap().pending(), "{revoke}");
        match revoke {
            "permission" => {
                sqlx::query(
                    "UPDATE auth_sessions SET can_send_commands=0 WHERE token='fill-token'",
                )
                .execute(&f.state.auth_db)
                .await
                .unwrap();
            }
            "session" => {
                sqlx::query("DELETE FROM auth_sessions WHERE token='fill-token'")
                    .execute(&f.state.auth_db)
                    .await
                    .unwrap();
            }
            "stop" => {
                f.state.manual_abort_intent.request();
            }
            "reset" => {
                let old = crate::gse::automatic_generation(&f.state);
                assert_eq!(
                    crate::gse::reset_generation::invalidate_capture_epoch(&f.state, old),
                    Ok(old + 1)
                );
            }
            "recording" => {
                f.state
                    .db_queue_tx
                    .send(crate::telemetry_db::DbQueueItem::Control(
                        crate::telemetry_db::RecordingCommand::Stop,
                    ))
                    .await
                    .unwrap();
                for _ in 0..200 {
                    if f.state.recording_status_snapshot().mode == RecordingModeWire::Idle {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                assert_eq!(
                    f.state.recording_status_snapshot().mode,
                    RecordingModeWire::Idle
                );
                assert_eq!(f.state.fill_recording.ready_instance(), 0);
            }
            _ => unreachable!(),
        }
        feed_zero(&f, Duration::from_millis(1), |_| {}).await;
        settle_zero(&f, false).await;
        assert!(!f.state.auto_zero.lock().unwrap().pending(), "{revoke}");
        assert!(
            f.sent.lock().unwrap().is_empty(),
            "{revoke}: late Fill output"
        );
        assert_eq!(
            f.state.loadcell_calibration.lock().unwrap().ch1_zero_raw,
            None,
            "{revoke}: revoked tare committed"
        );
        assert!(
            !f.state.abort_indicator_latched(),
            "{revoke}: revocation fabricated Abort"
        );
        if revoke == "stop" {
            assert!(f.state.manual_abort_intent.pending_epoch().is_some());
        }
    }
}
