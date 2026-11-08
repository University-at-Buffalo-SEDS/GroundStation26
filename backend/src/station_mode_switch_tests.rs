//! Route and admission tests. They run unchanged in every build: the current
//! mode is this build's mode and the installed set is chosen per test.
use super::*;
use crate::recording_export::tests::{TestDirectory, export_state};
use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

fn other_mode() -> Mode {
    if local_mode() == Mode::Hitl {
        Mode::TestFire
    } else {
        Mode::Hitl
    }
}
fn config(root: &Path, rrls: bool, supervised: bool) -> Config {
    Config {
        rrls,
        supervised,
        flight_sim: false,
        dir: root.join("station-mode"),
        available: parse_available("normal,hitl,test_fire,testing"),
        current: Target {
            mode: local_mode(),
            simulation: false,
        },
        restart_delay: Duration::ZERO,
    }
}
struct Fixture {
    _root: TestDirectory,
    dir: PathBuf,
    state: Arc<AppState>,
    switch: Arc<Switch>,
    signal: Arc<RestartSignal>,
}
async fn fixture_with(rrls: bool, supervised: bool) -> Fixture {
    let root = TestDirectory::new();
    let state = export_state(&root.0, false).await;
    std::fs::write(
        root.0.join("auth.json"),
        json!({"users":[
            {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
            {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true},"command_access":{"allowed_commands":["Abort","StationMode"]}},
            {"username":"commander","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true},"command_access":{"allowed_commands":["Abort"]}},
            {"username":"viewer","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}}
        ]})
        .to_string(),
    )
    .unwrap();
    for name in ["administrator", "operator", "commander", "viewer"] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
            .bind(format!("{name}-token"))
            .bind(name)
            .execute(&state.auth_db)
            .await
            .unwrap();
    }
    let signal = Arc::new(RestartSignal::default());
    let dir = root.0.join("station-mode");
    let switch = Arc::new(Switch::new(config(&root.0, rrls, supervised), signal.clone()));
    Fixture {
        _root: root,
        dir,
        state,
        switch,
        signal,
    }
}
async fn fixture() -> Fixture {
    fixture_with(false, true).await
}
impl Fixture {
    async fn call(&self, method: &str, path: &str, user: &str, body: Option<Value>) -> (StatusCode, Value) {
        let app = routes_with(Some(self.switch.clone())).with_state(self.state.clone());
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("Authorization", format!("Bearer {user}-token"));
        if body.is_some() {
            request = request.header("Content-Type", "application/json");
        }
        let request = request
            .body(body.map_or_else(Body::empty, |v| Body::from(v.to_string())))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn snapshot(&self, user: &str) -> Value {
        let (status, value) = self.call("GET", "/api/station-modes", user, None).await;
        assert_eq!(status, StatusCode::OK);
        value
    }
    fn binding(snapshot: &Value, target: &str) -> Value {
        json!({
            "backend_instance": snapshot["backend_instance"],
            "principal_context": snapshot["principal_context"],
            "authority_id": snapshot["authority_id"],
            "revision": snapshot["revision"],
            "policy_epoch": snapshot["policy_epoch"],
            "current_id": snapshot["current"]["id"],
            "target_id": target,
        })
    }
    async fn prepare(&self, user: &str, target: &str) -> (StatusCode, Value) {
        let snapshot = self.snapshot(user).await;
        self.call("POST", "/api/station-modes/prepare", user, Some(Self::binding(&snapshot, target)))
            .await
    }
    fn confirmation(challenge: &Value, phrase: &str) -> Value {
        let mut body = challenge.clone();
        let object = body.as_object_mut().unwrap();
        for key in ["current", "target", "summary", "required_confirmation", "expires_in_ms"] {
            object.remove(key);
        }
        object.insert("confirmation".into(), phrase.into());
        body
    }
    fn selection(&self) -> Option<Selection> {
        read_selection(&self.dir)
    }
}

#[tokio::test]
async fn snapshot_follows_the_ui_v2_contract_with_restart_transitions() {
    let f = fixture().await;
    let s = f.snapshot("administrator").await;
    assert_eq!(s["version"], 2);
    for key in ["current", "transition", "policy_epoch", "reason"] {
        assert!(s.as_object().unwrap().contains_key(key), "{key}");
    }
    assert_eq!(s["epoch_fencing_supported"], false, "restart is the fence");
    assert_eq!(s["admissions_open"], true, "never closes ordinary command admission");
    assert_eq!(s["current"]["mode_id"], mode_id(local_mode()));
    assert_eq!(s["change_supported"], true);
    assert_eq!(s["permissions"]["can_change"], true);
    assert_eq!(s["principal_context"], "user:administrator");
    let ids: Vec<_> = s["options"].as_array().unwrap().iter().map(|o| o["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, ["normal", "hitl", "test_fire", "testing", "testing-simulation"]);
    for option in s["options"].as_array().unwrap() {
        assert_eq!(option["transition_kind"], "restart");
        assert_eq!(option["available"], true);
        assert!(s["modes"].as_array().unwrap().iter().any(|m| m["id"] == option["mode_id"]));
    }
    // Same installation across restarts; a new epoch per process.
    let again = Switch::new(f.switch.config.clone(), Arc::new(RestartSignal::default()));
    assert_eq!(again.backend_instance(), f.switch.backend_instance());
    assert_ne!(again.epoch, f.switch.epoch);
}

#[tokio::test]
async fn only_installed_builds_are_offered_and_unsupervised_backends_refuse() {
    let f = fixture_with(false, false).await;
    let s = f.snapshot("administrator").await;
    assert_eq!(s["change_supported"], false);
    assert!(s["reason"].as_str().unwrap().contains("supervisor"));
    let (status, body) = f.prepare("administrator", mode_id(other_mode())).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("supervisor"), "{body}");

    let root = TestDirectory::new();
    let mut only = config(&root.0, false, true);
    only.available = vec![local_mode(), other_mode()];
    let switch = Switch::new(only, Arc::new(RestartSignal::default()));
    let targets: Vec<_> = switch.config.targets().iter().map(|t| t.id()).collect();
    assert!(targets.contains(&mode_id(other_mode()).to_owned()));
    assert!(!targets.iter().any(|t| t.ends_with("-simulation")) || local_mode() == Mode::Testing);
}

#[tokio::test]
async fn permission_requires_admin_or_operator_allowed_station_mode() {
    let f = fixture().await;
    for (user, allowed) in [("administrator", true), ("operator", true), ("commander", false), ("viewer", false)] {
        let s = f.snapshot(user).await;
        assert_eq!(s["permissions"]["can_change"], allowed, "{user}");
        let (status, body) = f.prepare(user, mode_id(other_mode())).await;
        if allowed {
            assert_eq!(status, StatusCode::OK, "{user} {body}");
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN, "{user}");
            assert!(body["error"].as_str().unwrap().contains("StationMode"));
        }
    }
    let app = routes_with(Some(f.switch.clone())).with_state(f.state.clone());
    let response = app
        .oneshot(Request::builder().uri("/api/station-modes").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(matches!(response.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn confirmed_switch_saves_the_selection_then_requests_a_clean_restart() {
    let f = fixture().await;
    let target = mode_id(other_mode());
    let (status, challenge) = f.prepare("operator", target).await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    assert_eq!(challenge["target"]["id"], target);
    assert_eq!(challenge["expires_in_ms"], 15_000);
    let phrase = challenge["required_confirmation"].as_str().unwrap().to_owned();
    assert_eq!(phrase, format!("RESTART {}", target.to_uppercase()));
    // A wrong phrase consumes the ticket and changes nothing.
    let (status, body) = f
        .call("POST", "/api/station-modes/confirm", "operator", Some(Fixture::confirmation(&challenge, "restart")))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = f
        .call("POST", "/api/station-modes/confirm", "operator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "single-use ticket");
    assert!(f.selection().is_none());
    assert!(!f.signal.requested());
    // Another user cannot use this operator's review.
    let (_, challenge) = f.prepare("operator", target).await;
    let (status, _) = f
        .call("POST", "/api/station-modes/confirm", "administrator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, challenge) = f.prepare("operator", target).await;
    // A request naming some other ticket cannot retire the pending review.
    let mut forged = Fixture::confirmation(&challenge, &phrase);
    forged["ticket"] = "0".repeat(32).into();
    let (status, _) = f.call("POST", "/api/station-modes/confirm", "viewer", Some(forged)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, receipt) = f
        .call("POST", "/api/station-modes/confirm", "operator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["transition"]["state"], "pending");
    assert_eq!(receipt["transition"]["id"], challenge["transition_id"]);
    let selection = f.selection().unwrap();
    assert_eq!(selection.mode, target);
    assert!(!selection.simulation);
    assert_eq!(selection.role, "primary");
    assert_eq!(selection.requested_by, "user:operator");
    for _ in 0..100 {
        if f.signal.requested() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(f.signal.requested(), "clean shutdown requested");
    // Further reviews are refused while the restart runs.
    let (status, body) = f.prepare("operator", target).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("in progress"));
    assert_eq!(f.snapshot("operator").await["transition"]["state"], "pending");
}

#[tokio::test]
async fn busy_station_refuses_with_the_server_reason() {
    use crate::telemetry_db::{LaunchClockKind, LaunchClockMsg};
    use crate::types::FlightState;
    let f = fixture().await;
    let target = mode_id(other_mode());
    *f.state.launch_clock.lock().unwrap() = LaunchClockMsg {
        kind: LaunchClockKind::TMinus,
        anchor_timestamp_ms: Some(1),
        duration_ms: Some(10_000),
    };
    let s = f.snapshot("administrator").await;
    assert_eq!(s["options"][0]["available"], false);
    assert!(s["options"][0]["reason"].as_str().unwrap().contains("T clock"));
    assert!(s["reason"].as_str().unwrap().contains("T clock"));
    let (status, body) = f.prepare("administrator", target).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("T clock"), "{body}");
    *f.state.launch_clock.lock().unwrap() = LaunchClockMsg::idle();

    *f.state.state.lock().unwrap() = FlightState::Armed;
    let (status, body) = f.prepare("administrator", target).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("Armed"), "{body}");
    *f.state.state.lock().unwrap() = FlightState::Idle;

    f.state.launch_sequence_command_pending.store(true, Ordering::SeqCst);
    let (status, body) = f.prepare("administrator", target).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("Launch"), "{body}");
    f.state.launch_sequence_command_pending.store(false, Ordering::SeqCst);

    // Idle again: admitted. Activity between review and confirm is refused
    // at confirm and nothing is saved.
    let (status, challenge) = f.prepare("administrator", target).await;
    assert_eq!(status, StatusCode::OK);
    *f.state.state.lock().unwrap() = FlightState::NitrousFill;
    let phrase = challenge["required_confirmation"].as_str().unwrap().to_owned();
    let (status, body) = f
        .call("POST", "/api/station-modes/confirm", "administrator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("NitrousFill"), "{body}");
    assert!(f.selection().is_none());
    assert!(!f.signal.requested());
}

#[tokio::test]
async fn activity_just_before_restart_cancels_it_and_restores_the_selection() {
    let f = fixture().await;
    let previous = Selection {
        version: 1,
        role: "primary".into(),
        mode: mode_id(local_mode()).into(),
        simulation: false,
        target_id: mode_id(local_mode()).into(),
        transition_id: "older".into(),
        requested_by: "user:administrator".into(),
        requested_at_ms: 1,
    };
    write_durable(&f.dir.join(SELECTION_FILE), &serde_json::to_vec(&previous).unwrap()).unwrap();
    let mut slow = f.switch.config.clone();
    slow.restart_delay = Duration::from_millis(300);
    let switch = Arc::new(Switch::new(slow, f.signal.clone()));
    let f = Fixture { switch, ..f };
    let target = mode_id(other_mode());
    let (_, challenge) = f.prepare("administrator", target).await;
    let phrase = challenge["required_confirmation"].as_str().unwrap().to_owned();
    let (status, _) = f
        .call("POST", "/api/station-modes/confirm", "administrator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.selection().unwrap().mode, target);
    *f.state.state.lock().unwrap() = crate::types::FlightState::PreFill;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!f.signal.requested(), "an active station is never restarted");
    assert_eq!(f.selection().unwrap(), previous, "previous selection restored");
    let s = f.snapshot("administrator").await;
    assert_eq!(s["transition"]["state"], "failed");
    assert!(s["transition"]["error"].as_str().unwrap().contains("PreFill"));
    *f.state.state.lock().unwrap() = crate::types::FlightState::Idle;
    let (status, _) = f.prepare("administrator", target).await;
    assert_eq!(status, StatusCode::OK, "a cancelled restart can be retried");
}

#[tokio::test]
async fn restarted_process_reports_the_saved_transition_as_applied_or_failed() {
    let root = TestDirectory::new();
    let cfg = config(&root.0, false, true);
    let mut selection = Selection {
        version: 1,
        role: "primary".into(),
        mode: mode_id(local_mode()).into(),
        simulation: false,
        target_id: mode_id(local_mode()).into(),
        transition_id: "switch-abc".into(),
        requested_by: "user:operator".into(),
        requested_at_ms: 1,
    };
    write_durable(&cfg.dir.join(SELECTION_FILE), &serde_json::to_vec(&selection).unwrap()).unwrap();
    let switch = Switch::new(cfg.clone(), Arc::new(RestartSignal::default()));
    let transition = switch.runtime.lock().unwrap().transition.clone().unwrap();
    assert_eq!((transition.id.as_str(), transition.state), ("switch-abc", "applied"));
    selection.target_id = format!("{}-simulation", mode_id(other_mode()));
    write_durable(&cfg.dir.join(SELECTION_FILE), &serde_json::to_vec(&selection).unwrap()).unwrap();
    let switch = Switch::new(cfg.clone(), Arc::new(RestartSignal::default()));
    let transition = switch.runtime.lock().unwrap().transition.clone().unwrap();
    assert_eq!(transition.state, "failed");
    assert!(transition.error.unwrap().contains("installed"));
    // An RRLS selection never describes the primary.
    selection.role = "rrls".into();
    write_durable(&cfg.dir.join(SELECTION_FILE), &serde_json::to_vec(&selection).unwrap()).unwrap();
    assert!(Switch::new(cfg, Arc::new(RestartSignal::default())).runtime.lock().unwrap().transition.is_none());
}

#[tokio::test]
async fn rrls_offers_a_simulated_primary_for_every_build_and_switches_only_itself() {
    let f = fixture_with(true, true).await;
    let s = f.snapshot("administrator").await;
    let ids: Vec<_> = s["options"].as_array().unwrap().iter().map(|o| o["id"].as_str().unwrap().to_owned()).collect();
    for mode in ["normal", "hitl", "test_fire", "testing"] {
        assert!(ids.contains(&mode.to_owned()));
        assert!(ids.contains(&format!("{mode}-simulation")));
    }
    assert!(s["reason"].as_str().unwrap().contains("never changed"));
    let sim = s["options"].as_array().unwrap().iter().find(|o| o["simulation"] == true).unwrap();
    assert!(sim["label"].as_str().unwrap().contains("SIMULATED"));
    assert!(sim["output_domain"].as_str().unwrap().contains("no radio output"));
    let target = format!("{}-simulation", mode_id(local_mode()));
    let (status, challenge) = f.prepare("administrator", &target).await;
    assert_eq!(status, StatusCode::OK, "{challenge}");
    assert!(challenge["summary"].as_str().unwrap().contains("only this RRLS"));
    let phrase = challenge["required_confirmation"].as_str().unwrap().to_owned();
    let (status, _) = f
        .call("POST", "/api/station-modes/confirm", "administrator", Some(Fixture::confirmation(&challenge, &phrase)))
        .await;
    assert_eq!(status, StatusCode::OK);
    let selection = f.selection().unwrap();
    assert_eq!((selection.role.as_str(), selection.simulation), ("rrls", true));
}

#[test]
fn target_ids_roundtrip_and_simulation_is_mode_specific_on_the_primary() {
    for mode in ALL_MODES {
        for simulation in [false, true] {
            let target = Target { mode, simulation };
            assert_eq!(Target::parse(&target.id()), Some(target));
        }
        assert_eq!(simulation_supported(mode, false), mode == Mode::Testing);
        assert!(simulation_supported(mode, true));
    }
    assert_eq!(Target::parse("flight"), None);
    assert_eq!(parse_available(" hitl,bogus,normal,hitl"), vec![Mode::Normal, Mode::Hitl]);
}
