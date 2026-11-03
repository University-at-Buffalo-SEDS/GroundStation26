//! Native handler/route fixture: in-memory SDK endpoints only; no backend listener.
use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use tower::ServiceExt;

#[tokio::test]
async fn actual_routes_require_current_permission_run_and_epoch_and_wait_for_confirmation() {
    const CHILD: &str = "GS_SEQUENCE_PROGRESS_ROUTES_CHILD";
    const NAME: &str = "gse::progress::tests::actual_routes_require_current_permission_run_and_epoch_and_wait_for_confirmation";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_RRLS_ROLE", "off")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned sequence route fixture exceeded 30 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in text.lines() {
            if let Some(start) = line.find("SEQUENCE_CONTROL ") {
                println!("{}", &line[start..]);
            }
        }

        assert!(output.status.success(), "{text}");
        assert!(
            text.contains("1 passed"),
            "child did not execute exact test: {text}"
        );
        return;
    }
    let root = crate::recording_export::tests::TestDirectory::new();
    unsafe {
        std::env::set_var("GS_NETWORK_VARIABLE_CACHE", root.0.join("variables.json"));
        std::env::set_var("GS_CHECKLISTS_FILE", root.0.join("checklists.json"));
    }
    let (mut state, sent, peer) = crate::gse::tests::button_test_state().await;
    let path = root.0.join("auth.json");
    std::fs::write(&path,serde_json::json!({"version":1,"anonymous":{"view_data":true,"send_commands":false},"users":[{"username":"fixture-admin","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}]}).to_string()).unwrap();
    Arc::get_mut(&mut state).unwrap().auth = Arc::new(crate::auth::AuthManager::new(path));
    crate::ensure_auth_sessions_table(&state.auth_db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES('fixture-progress-token','fixture-admin','session',1,1,0,9999999999999)").execute(&state.auth_db).await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer fixture-progress-token".parse().unwrap(),
    );
    let mut policy = state.action_policy_snapshot();
    policy.key_enabled = true;
    policy.software_buttons_enabled = true;
    state.set_action_policy(policy);
    let reserved = reserve(&state).unwrap();
    let key = reserved.0.clone();
    {
        let mut rt = state.gse.lock().unwrap();
        rt.engine.config.dry_self_test_confirmed = true;
        rt.engine.config.pressure_ceiling_psi = Some(200.);
        let input = rt.input(true, true);
        rt.engine.request(Action::SelfTest, input).unwrap();
        rt.progress_start(Some(reserved), Kind::ValveSelfTest, input);
    }
    flush(&state);
    let router = routes().with_state(state.clone());
    let make = |key: &RunKey, authenticated: bool| {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/gse/sequences/cancel")
            .header("content-type", "application/json");
        if authenticated {
            request = request.header("authorization", "Bearer fixture-progress-token");
        }
        request
            .body(Body::from(serde_json::to_vec(key).unwrap()))
            .unwrap()
    };
    let denied = router.clone().oneshot(make(&key, false)).await.unwrap();
    assert!(matches!(
        denied.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
    let mut replaced = key.clone();
    replaced.run_id = "f".repeat(32);
    assert_eq!(
        router
            .clone()
            .oneshot(make(&replaced, true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    *state.state.lock().unwrap() = FlightState::Aborted;
    assert_eq!(
        router
            .clone()
            .oneshot(make(&key, true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT,
        "the existing flight-state command gate is mandatory"
    );
    *state.state.lock().unwrap() = FlightState::Idle;
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(make(&key, true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT,
        "retained Fire/launch inhibition cannot be bypassed"
    );
    *state.launch_history.lock().unwrap() = crate::launch_history::Store::ephemeral();
    assert!(
        sent.lock().unwrap().is_empty(),
        "rejected intents must not dispatch"
    );
    let response = router.clone().oneshot(make(&key, true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 32768).await.unwrap()).unwrap();
    assert_eq!(body["completion"], "pending_authoritative_confirmation");
    assert_eq!(
        state.gse.lock().unwrap().engine.status.phase,
        Phase::CancelSetup
    );
    assert_eq!(
        state
            .gse
            .lock()
            .unwrap()
            .progress
            .current()
            .unwrap()
            .outcome,
        wire::Outcome::Cancelling
    );
    crate::gse::tests::pump_test_link(&state, &peer);
    let before = sent.lock().unwrap().len();
    assert!(before > 0);
    assert_eq!(
        router
            .clone()
            .oneshot(make(&key, true))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    crate::gse::tests::pump_test_link(&state, &peer);
    assert_eq!(
        sent.lock().unwrap().len(),
        before,
        "repeated cancel cannot restart the safe sequence"
    );
    let missing = format!(
        "/api/gse/sequences/results/{}/{}/{}",
        key.backend_instance, key.reset_epoch, key.run_id
    );
    assert_eq!(
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&missing)
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::GONE,
        "running is not a completed result"
    );
    // Explicit fixture interruption is an actual producer terminal event; it
    // does not claim the still-running safe cancellation was confirmed.
    state.gse.lock().unwrap().progress.complete(
        TerminalProof::Interrupted,
        crate::sequence_progress_service::now_ms(),
    );
    flush(&state);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&missing)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: wire::Snapshot =
        serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap();
    assert_eq!(result.outcome, wire::Outcome::Interrupted);
    let id = result.notification_id.unwrap();
    let notification = state
        .notifications
        .lock()
        .unwrap()
        .iter()
        .find(|n| n.id == id)
        .cloned()
        .unwrap();
    assert_eq!(
        Some(notification.timestamp_ms),
        result.notification_timestamp_ms
    );
    assert!(notification.message.starts_with("Warning: "));
    // Capture before authentication and compare current reset generation.
    state
        .automatic_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    assert_eq!(
        router.oneshot(make(&key, true)).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    // A real active trim is PauseSetup in Engine, but still owns the fill run.
    let trim_reservation = reserve(&state).unwrap();
    let trim_key = trim_reservation.0.clone();
    {
        let mut rt = state.gse.lock().unwrap();
        rt.engine = Engine::default();
        rt.engine.config.pressure_ceiling_psi = Some(200.);
        rt.engine.config.maximum_zero_offset_psi = Some(8.);
        rt.observe_pressure(Some(0.));
        rt.observe_mass(Some(10.2));
        let input = rt.input(true, true);
        let (trim, _) =
            gse_sequence::fill_trim::Controller::start(input.now_ms, 10., 200., &rt.engine.config)
                .unwrap();
        rt.engine.status.phase = Phase::PauseSetup;
        rt.progress_start(Some(trim_reservation), Kind::Fill, input);
        let view = rt.engine.progress_observation(input);
        let mass = rt.mass;
        rt.progress.observe_trim(
            trim.progress_observation(),
            view,
            mass,
            input.now_ms,
            None,
            crate::sequence_progress_service::now_ms(),
        );
        rt.fill_trim = Some(trim);
    }
    flush(&state);
    let trim_router = routes().with_state(state.clone());
    let response = trim_router.oneshot(make(&trim_key, true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(state.gse.lock().unwrap().fill_trim.is_none());
    assert_eq!(
        state.gse.lock().unwrap().engine.status.phase,
        Phase::CancelSetup
    );
    assert_eq!(
        state
            .gse
            .lock()
            .unwrap()
            .progress
            .current()
            .unwrap()
            .outcome,
        wire::Outcome::Cancelling
    );
    println!(
        "SEQUENCE_CONTROL active_trim_safe_cancel=ACCEPTED completion=pending_confirmed_engine_transition"
    );
    let reset_state = crate::state::tests::test_app_state().await;
    let reset_reserved = reserve(&reset_state).unwrap();
    let interrupted_key = reset_reserved.0.clone();
    {
        let mut rt = reset_state.gse.lock().unwrap();
        rt.engine.config.dry_self_test_confirmed = true;
        rt.engine.config.pressure_ceiling_psi = Some(200.);
        rt.engine.config.maximum_zero_offset_psi = Some(8.);
        rt.observe_pressure(Some(0.));
        let input = rt.input(true, true);
        rt.engine.request(Action::SelfTest, input).unwrap();
        rt.progress_start(Some(reset_reserved), Kind::ValveSelfTest, input);
    }
    flush(&reset_state);
    reset_state.clear_runtime_data_for_sim_reset();
    {
        let reset_service = reset_state.sequence_progress.lock().unwrap();
        assert!(
            reset_service
                .envelope(
                    automatic_generation(&reset_state),
                    crate::sequence_progress_service::now_ms()
                )
                .active
                .is_empty()
        );
        assert_eq!(
            reset_service.result(&interrupted_key).unwrap().outcome,
            wire::Outcome::Interrupted
        );
        assert!(
            reset_service
                .result(&interrupted_key)
                .unwrap()
                .notification_id
                .is_some()
        );
        assert_ne!(
            reset_service
                .reserve(automatic_generation(&reset_state))
                .unwrap(),
            interrupted_key
        );
    }
    {
        let mut rt = reset_state.gse.lock().unwrap();
        rt.test_now_ms = Some(1234);
        rt.pressure = Some(Sample {
            value: 5.,
            at_ms: 1234,
        });
        rt.valve_pressure = Some(Sample {
            value: 12.,
            at_ms: 1234,
        });
        rt.engine.status.phase = Phase::Paused;
        for pending in [false, true] {
            rt.turbo_inventory_pending = pending;
            let original = rt.input(true, false);
            let copied = rt.input_at(rt.presentation_now(None), true, false);
            assert_eq!(original.now_ms, copied.now_ms);
            assert_eq!(original.prelaunch, copied.prelaunch);
            assert_eq!(original.interlock, copied.interlock);
            assert_eq!(
                original.pressure.map(|p| (p.value.to_bits(), p.at_ms)),
                copied.pressure.map(|p| (p.value.to_bits(), p.at_ms))
            );
            assert_eq!(
                original.valves.map(|v| v.map(|v| (v.open, v.at_ms))),
                copied.valves.map(|v| v.map(|v| (v.open, v.at_ms)))
            );
        }
        rt.test_now_ms = None;
        assert_eq!(
            rt.presentation_now(Some(0)),
            0,
            "known model zero remains zero, not a fabricated missing clock"
        );
    }
    fn seed_reporting(state: &AppState) {
        *state.state.lock().unwrap() = FlightState::Idle;
        let reservation = reserve(state).unwrap();
        let mut rt = state.gse.lock().unwrap();
        rt.test_now_ms = Some(100);
        rt.engine.config.dry_self_test_confirmed = true;
        rt.engine.config.pressure_ceiling_psi = Some(200.);
        rt.engine.config.maximum_zero_offset_psi = Some(8.);
        rt.observe_pressure(Some(0.));
        let input = rt.input(true, true);
        rt.engine.request(Action::SelfTest, input).unwrap();
        rt.progress_start(Some(reservation), Kind::ValveSelfTest, input);
    }
    fn next_observation(state: &AppState) {
        let mut rt = state.gse.lock().unwrap();
        let input = rt.input_at(100, true, true);
        rt.progress_observe(input, Cause::Tick);
    }
    let old = crate::state::tests::test_app_state().await;
    seed_reporting(&old);
    flush(&old);
    next_observation(&old);
    let older = old.gse.lock().unwrap().progress.take();
    next_observation(&old);
    let newer = old.gse.lock().unwrap().progress.take();
    let older = old
        .sequence_progress
        .projector
        .lock()
        .unwrap()
        .apply(older)
        .0;
    let newer = old
        .sequence_progress
        .projector
        .lock()
        .unwrap()
        .apply(newer)
        .0;
    publish(&old, newer, None);
    publish(&old, older, None);
    let old_result = envelope(&old);
    assert!(
        old_result["reporting_error"]
            .as_str()
            .unwrap()
            .contains("RevisionGap")
    );
    println!(
        "SEQUENCE_CONTROL old_split_drain=RED journal_revision={} error={}",
        old_result["active"][0]["revision"], old_result["reporting_error"]
    );

    let ordered = crate::state::tests::test_app_state().await;
    seed_reporting(&ordered);
    flush(&ordered);
    let gate = publication_gate(&ordered).unwrap();
    let lease = publication_lease(&ordered, &gate);
    next_observation(&ordered);
    let older = ordered.gse.lock().unwrap().progress.take();
    next_observation(&ordered);
    let waiting = ordered.clone();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        flush(&waiting);
        done_tx.send(()).unwrap();
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(20))
            .is_err()
    );
    assert!(
        ordered.gse.try_lock().is_ok(),
        "publication waiter must hold no GSE lock"
    );
    project_and_publish(&ordered, older, None);
    drop(lease);
    done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    waiter.join().unwrap();
    let observed = envelope(&ordered);
    assert!(observed["reporting_error"].is_null());
    assert_eq!(observed["active"][0]["revision"], 3);
    println!("SEQUENCE_CONTROL serialized_publication=GREEN revision=3 waiting_holds_gse=false");

    #[cfg(unix)]
    {
        use std::sync::atomic::Ordering;
        let observed = crate::state::tests::test_app_state().await;
        seed_reporting(&observed);
        flush(&observed);
        for old_under_dispatch in [true, false] {
            let path = root.0.join(if old_under_dispatch {
                "old-report-clock.fifo"
            } else {
                "new-report-clock.fifo"
            });
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            let probe = Arc::new(ReportClockProbe {
                path: path.clone(),
                started: std::sync::atomic::AtomicBool::new(false),
            });
            *REPORT_CLOCK_PROBE.lock().unwrap() = Some(probe.clone());
            let work = observed.clone();
            let waiter = std::thread::spawn(move || {
                if old_under_dispatch {
                    let _gate = DISPATCH_GATE.lock().unwrap();
                    project_and_publish(
                        &work,
                        crate::sequence_progress_stage::Batch::empty(),
                        None,
                    );
                } else {
                    flush(&work);
                }
            });
            let end = Instant::now() + std::time::Duration::from_secs(2);
            while !probe.started.load(Ordering::Acquire) {
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
            if old_under_dispatch {
                assert!(DISPATCH_GATE.try_lock().is_err());
                println!(
                    "SEQUENCE_CONTROL old_report_clock_under_dispatch=RED actual_fifo_read=blocked"
                );
            } else {
                assert!(DISPATCH_GATE.try_lock().is_ok());
                assert!(observed.gse.try_lock().is_ok());
                let ticket = LaunchAdmission::capture(&observed);
                let at = Instant::now();
                let mut admitted = false;
                with_stop_admission(&observed, ticket, || {
                    admitted = true;
                })
                .unwrap();
                assert!(admitted);
                assert_eq!(observed.manual_abort_intent.request_untimed(), 1);
                assert!(
                    with_stop_admission(&observed, ticket, || panic!("stale output admitted"))
                        .is_err()
                );
                assert!(at.elapsed() < std::time::Duration::from_millis(100));
                println!(
                    "SEQUENCE_CONTROL deferred_report_clock=GREEN actual_fifo_read=blocked dispatch_free=true gse_free=true controller_admitted_before_stop=true stale_controller_refused=true manual_stop_prompt=true"
                );
            }
            std::fs::write(&path, "100").unwrap();
            waiter.join().unwrap();
            *REPORT_CLOCK_PROBE.lock().unwrap() = None;
        }
    }

    let poisoned = crate::state::tests::test_app_state().await;
    seed_reporting(&poisoned);
    flush(&poisoned);
    let gate = publication_gate(&poisoned).unwrap();
    let poison = std::thread::spawn(move || {
        let _lease = gate.lock().unwrap();
        panic!("fixture metadata-only publisher interruption");
    });
    assert!(poison.join().is_err());
    let before = poisoned.gse.lock().unwrap().engine.status.phase;
    flush(&poisoned);
    assert_eq!(poisoned.gse.lock().unwrap().engine.status.phase, before);
    assert!(envelope(&poisoned)["reporting_error"].is_string());
    assert_eq!(*poisoned.state.lock().unwrap(), FlightState::Idle);
    println!("SEQUENCE_CONTROL poisoned_publisher=UNAVAILABLE engine_preserved=true no_abort=true");
}
