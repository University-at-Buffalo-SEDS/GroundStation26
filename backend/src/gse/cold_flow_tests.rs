//! Actual routes, physical-edge sender, public SDK controller packet path and
//! source engine tick. Disposable memory endpoints only: no GPIO/serial/service.
use super::*;
use axum::{
    body::{Body, to_bytes},
    http::Request as HttpRequest,
};
use gse_sequence::{CLOSED, RELIEVED};
use tower::ServiceExt;

struct Fixture {
    _recording: crate::telemetry_task::recording_owner::TestOwner,
    _root: crate::recording_export::tests::TestDirectory,
    state: Arc<AppState>,
    sent: super::super::tests::TestCommands,
    peer: Arc<sedsnet::router::Router>,
    users: std::path::PathBuf,
    actual: [bool; 5],
    cursor: usize,
    packets: Arc<std::sync::Mutex<Vec<sedsnet::packet::Packet>>>,
}
impl Fixture {
    async fn new() -> Self { Self::configured(false).await }
    async fn configured(controlled_core: bool) -> Self {
        let root = crate::recording_export::tests::TestDirectory::new();
        unsafe {
            std::env::set_var("GS_NETWORK_VARIABLE_CACHE", root.0.join("vars.json"));
            std::env::set_var("GS_CHECKLISTS_FILE", root.0.join("checklists.json"));
            std::env::set_var("GS_GSE_CONFIG", root.0.join("gse.json"));
        }
        let (mut state, sent, mut peer) = super::super::tests::button_test_state().await;
        let packets = Arc::new(std::sync::Mutex::new(Vec::new()));
        if controlled_core {
            let owner = Arc::as_ptr(&state) as usize;
            let handlers = [("VALVE_BOARD", "VALVE_COMMAND"), ("ACTUATOR_BOARD", "ACTUATOR_COMMAND")].map(|(endpoint, kind)| {
                let sent = sent.clone(); let packets = packets.clone();
                sedsnet::router::EndpointHandler::new_packet_handler(crate::telemetry_schema::endpoint(endpoint), move |packet| {
                    if packet.data_type() != crate::telemetry_schema::data_type(kind) { return Ok(()); }
                    sent.lock().unwrap().push((kind.into(), packet.payload().to_vec()));
                    packets.lock().unwrap().push(packet.clone());
                    Ok(())
                })
            });
            peer = Arc::new(sedsnet::router::Router::new(
                sedsnet::router::RouterConfig::new(handlers).with_sender("GSE_TEST"),
            ));
            let router = Arc::new(sedsnet::router::Router::new_with_clock(
                sedsnet::router::RouterConfig::new([]).with_sender("GS"),
                Box::new(move || {
                    super::super::nonwaiting_admission_tests::clock_pause(owner);
                    1000
                }),
            ));
            crate::telemetry_task::refresh_host_network_time(&router, crate::telemetry_task::get_current_timestamp_ms());
            let ingress = Arc::new(std::sync::OnceLock::new());
            let peer_ingress = ingress.clone(); let weak = Arc::downgrade(&router);
            let options = sedsnet::router::RouterSideOptions {
                reliable_enabled:false, link_local_enabled:true, ..Default::default()
            }.with_small_packet_transport(128);
            let peer_side = peer.add_side_packed_with_options("test_ground", move |bytes| {
                weak.upgrade().unwrap().rx_packed_from_side(bytes,*peer_ingress.get().unwrap())
            },options.clone());
            let receiver = peer.clone();
            let side=router.add_side_packed_with_options("test_fill",move|bytes| receiver.rx_packed_from_side(bytes,peer_side),options);
            ingress.set(side).unwrap();peer.announce_discovery().unwrap();peer.process_all_queues_with_timeout(0).unwrap();
            let state_mut = Arc::get_mut(&mut state).unwrap();
            state_mut.topology_router = Arc::new(std::sync::OnceLock::new());
            assert!(state_mut.topology_router.set(router).is_ok());
        }
        let users = root.0.join("users.json");
        write_users(&users, false);
        Arc::get_mut(&mut state).unwrap().auth =
            Arc::new(crate::auth::AuthManager::new(users.clone()));
        crate::ensure_auth_sessions_table(&state.auth_db)
            .await
            .unwrap();
        for (user, token) in [
            ("cold-origin", "cold-origin-token"),
            ("cold-other", "cold-other-token"),
        ] {
            sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)").bind(token).bind(user).execute(&state.auth_db).await.unwrap();
        }
        let recording = crate::telemetry_task::recording_owner::TestOwner::start(
            &mut state,
            &root.0.join("recordings"),
        );
        let mut policy = state.action_policy_snapshot();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        state.set_action_policy(policy);
        {
            let mut rt = state.gse.lock().unwrap();
            rt.test_now_ms = Some(100);
            rt.engine.config.cold_flow_target_psi = Some(100.);
            rt.engine.config.nitrogen_target_psi = 120.0; // Preserve this low-pressure test fixture.
            rt.engine.config.pressure_ceiling_psi = Some(200.);
            rt.engine.config.maximum_zero_offset_psi = Some(8.);
            rt.observe_pressure(Some(0.));
            for key in KEYS {
                rt.observe_valve(key, false);
            }
            #[cfg(not(feature = "hitl_mode"))]
            {
                rt.operational.startup_configured = true;
                rt.operational.topology_stable = true;
            }
        }
        Self {
            _recording: recording,
            _root: root,
            state,
            sent,
            peer,
            users,
            actual: CLOSED,
            cursor: 0,
            packets,
        }
    }
    async fn call(
        &self,
        path: &str,
        body: serde_json::Value,
        token: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let router = crate::gse::routes().with_state(self.state.clone());
        let mut request = HttpRequest::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = router
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let code = response.status();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (
            code,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| serde_json::json!({"detail":String::from_utf8_lossy(&bytes)})),
        )
    }
    async fn start(&self) -> RunKey {
        let (code, value) = self
            .call(
                "/api/gse/cold-flow/start",
                serde_json::json!({}),
                Some("cold-origin-token"),
            )
            .await;
        assert_eq!(code, StatusCode::OK, "{value}");
        serde_json::from_value(value["key"].clone()).unwrap()
    }
    fn phase(&self) -> ColdPhase {
        self.state
            .gse
            .lock()
            .unwrap()
            .cold_flow
            .as_ref()
            .unwrap()
            .controller
            .observation()
            .phase
    }
    fn pump(&mut self) {
        super::super::tests::pump_test_link(&self.state, &self.peer);
        let sent = self.sent.lock().unwrap();
        for (kind, bytes) in &sent[self.cursor..] {
            assert!(kind == "VALVE_COMMAND" || kind == "ACTUATOR_COMMAND");
            assert_eq!(bytes.len(), 1);
            assert_ne!(bytes[0] & 0x80, 0);
            let code = bytes[0] & 0x7f;
            let (index, on) = match (kind.as_str(), code) {
                ("VALVE_COMMAND", 0) => (0, true),
                ("VALVE_COMMAND", 1) => (1, true),
                ("VALVE_COMMAND", 2) => (2, true),
                ("VALVE_COMMAND", 3) => (0, false),
                ("VALVE_COMMAND", 4) => (1, false),
                ("VALVE_COMMAND", 5) => (2, false),
                ("ACTUATOR_COMMAND", 9) => (3, true),
                ("ACTUATOR_COMMAND", 12) => (3, false),
                ("ACTUATOR_COMMAND", 13) => (4, false),
                _ => panic!("unexpected cold-flow packet {kind} {code}"),
            };
            self.actual[index] = on;
        }
        self.cursor = sent.len();
    }
    fn step(&mut self, pressure: f32) {
        self.pump();
        {
            let mut rt = self.state.gse.lock().unwrap();
            let now = rt.test_now_ms.unwrap() + 10;
            rt.test_now_ms = Some(now);
            rt.observe_pressure(Some(pressure));
            for (i, key) in KEYS.into_iter().enumerate() {
                rt.observe_valve(key, self.actual[i]);
            }
        }
        crate::gse::tick(&self.state, true);
    }
    fn until(&mut self, phase: ColdPhase, pressure: f32) {
        for _ in 0..100 {
            if self.phase() == phase
                && self
                    .state
                    .gse
                    .lock()
                    .unwrap()
                    .cold_flow
                    .as_ref()
                    .unwrap()
                    .controller
                    .observation()
                    .waiting
                    .is_none()
            {
                return;
            }
            self.step(pressure);
        }
        panic!("wanted {phase:?}, got {:?}", self.phase());
    }
    fn hold(&mut self) {
        self.until(ColdPhase::Pressurizing, 0.);
        self.step(100.);
        self.until(ColdPhase::AwaitingRelease, 100.);
    }
    async fn edge(&self, intent: PanelIntent) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        crate::gpio_panel::queue_cold_flow_panel_edge(&tx, intent).unwrap();
        let TelemetryCommand::ColdFlowPanelRelease {
            key,
            generation,
            awaiting_at_edge,
        } = rx.recv().await.unwrap()
        else {
            panic!("physical cold edge became a normal Launch")
        };
        dispatch_panel(
            &self.state,
            PanelIntent {
                key,
                generation,
                awaiting_at_edge,
            },
        )
        .await;
    }
}
fn write_users(path: &std::path::Path, disabled: bool) {
    std::fs::write(path,serde_json::json!({"version":1,"anonymous":{"view_data":true,"send_commands":false},"users":[{"username":"cold-origin","roles":["admin"],"disabled":disabled,"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}},{"username":"cold-other","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}]}).to_string()).unwrap();
}
#[tokio::test]
async fn actual_cold_flow_routes_physical_edge_auth_cancel_and_no_launch_clock() {
    const NAME: &str = "gse::cold_flow::tests::actual_cold_flow_routes_physical_edge_auth_cancel_and_no_launch_clock";
    const CHILD: &str = "GS_COLD_FLOW_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_RRLS_ROLE", "off")
            .env("GS_SIM_MODE", "0")
            .env_remove("GS_SIM_CLOCK_FILE")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned cold-flow native fixture exceeded30s");
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
            if let Some(n) = line.find("COLD_FLOW_CONTROL ") {
                println!("{}", &line[n..]);
            }
        }
        assert!(output.status.success(), "{text}");
        assert!(text.contains("1 passed"));
        return;
    }
    // Real SDK packet path: N2 pressurize, isolated operator hold, edge-bound PILOT.
    let mut f = Fixture::new().await;
    let sent_before=f.sent.lock().unwrap().len();
    let current=serde_json::to_value(status(&f.state)).unwrap();
    assert_eq!(current["start_available"],true);
    assert_eq!(current["valid_for_ms"],2000);
    assert_eq!(current["backend_instance"],f.state.sequence_progress.backend_instance());
    assert_eq!(current["reset_epoch"],format!("{:016x}",automatic_generation(&f.state)));
    f.state.gse.lock().unwrap().test_now_ms=Some(1600);
    assert_eq!(serde_json::to_value(status(&f.state)).unwrap()["valid_for_ms"],500,"status cannot renew the pressure producer receipt");
    f.state.gse.lock().unwrap().test_now_ms=Some(2101);
    let expired=serde_json::to_value(status(&f.state)).unwrap();
    assert_eq!(expired["valid_for_ms"],0);assert_eq!(expired["start_available"],false);
    assert_eq!(f.sent.lock().unwrap().len(),sent_before,"status/lease reads never emit valve effects");
    f.state.gse.lock().unwrap().test_now_ms=Some(100);
    assert!(panel_edge(&f.state, automatic_generation(&f.state)).is_none());
    assert!(matches!(
        f.call("/api/gse/cold-flow/start", serde_json::json!({}), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ));
    f.state
        .gse
        .lock()
        .unwrap()
        .engine
        .config
        .cold_flow_target_psi = None;
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/start",
            serde_json::json!({}),
            Some("cold-origin-token")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    f.state
        .gse
        .lock()
        .unwrap()
        .engine
        .config
        .cold_flow_target_psi = Some(100.);
    let config = f.state.gse.lock().unwrap().engine.config.clone();
    let old_nitrogen = config.nitrogen_target_psi;
    assert_eq!(
        f.call(
            "/api/gse/config",
            serde_json::to_value(&config).unwrap(),
            Some("cold-origin-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    let saved: Config =
        serde_json::from_slice(&std::fs::read(super::super::config_path()).unwrap()).unwrap();
    assert_eq!(saved.cold_flow_target_psi, Some(100.));
    assert_eq!(saved.nitrogen_target_psi, old_nitrogen);
    f.state
        .gse
        .lock()
        .unwrap()
        .engine
        .config
        .cold_flow_target_psi = None;
    crate::gse::initialize(&f.state);
    assert_eq!(
        f.state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .cold_flow_target_psi,
        Some(100.)
    );
    #[cfg(not(feature = "hitl_mode"))]
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.operational.startup_configured = true;
        rt.operational.topology_stable = true;
    }
    // Actual save route: only the inert preference may retain current evidence.
    let base_config = f.state.gse.lock().unwrap().engine.config.clone();
    let install_evidence = || {
        let mut rt = f.state.gse.lock().unwrap();
        rt.engine.config = base_config.clone();
        let cap = gse_sequence::turbocharge::Capture {
            value: 100.,
            at_ms: 10,
        };
        rt.turbo_evidence = Some(gse_sequence::turbocharge::Evidence {
            context: super::turbocharge::fixture_context(&f.state, &rt),
            nitrous_target_mass_kg: 1.,
            pressure_deadband_psi: 12.,
            pressure_target_psi: 100.,
            max_added_nitrogen_mass_kg: 0.5,
            before_mass: cap,
            before_pressure: cap,
            reached_pressure: cap,
            after_mass: cap,
            after_pressure: cap,
            added_mass_kg: 0.,
            completed_at_ms: 10,
            mass_samples: 10,
            closed_valves_at_ms: [10; 5],
        });
        assert!(rt.turbo_evidence.as_ref().unwrap().valid());
    };
    install_evidence();
    let mut inert = base_config.clone();
    inert.cold_flow_target_psi = Some(95.);
    assert_eq!(
        f.call(
            "/api/gse/config",
            serde_json::to_value(&inert).unwrap(),
            Some("cold-origin-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(f.state.gse.lock().unwrap().turbo_evidence.is_some());
    type ConfigMutation = (&'static str, fn(&mut Config));
    let mutations: [ConfigMutation; 11] = [
        ("nitrogen_target", |c| c.nitrogen_target_psi += 1.),
        ("nitrogen_steps", |c| {
            c.nitrogen_steps = vec![gse_sequence::NitrogenStep {
                target_psi: 35.,
                settle_ms: 4000,
                hold_ms: 5000,
            }]
        }),
        ("fill_overfill", |c| c.fill_overfill_kg = 0.06),
        ("fill_tolerance", |c| c.fill_tolerance_kg = 0.06),
        ("fill_settle", |c| c.fill_settle_ms = 4000),
        ("turbo_mass", |c| {
            c.turbocharge_max_added_mass_kg = Some(0.3)
        }),
        ("pressure_step", |c| c.pressure_step_psi += 1.),
        ("pressure_ceiling", |c| c.pressure_ceiling_psi = Some(250.)),
        ("maximum_zero", |c| c.maximum_zero_offset_psi = Some(9.)),
        ("dry_consent", |c| c.dry_self_test_confirmed = false),
        ("grouped_panel", |c| c.grouped_panel = !c.grouped_panel),
    ];
    for (name, mutate) in mutations {
        install_evidence();
        // Exercise consent revocation, never grant through the bulk endpoint.
        if name == "dry_consent" {
            f.state
                .gse
                .lock()
                .unwrap()
                .engine
                .config
                .dry_self_test_confirmed = true;
        }
        let mut changed = f.state.gse.lock().unwrap().engine.config.clone();
        changed.cold_flow_target_psi = Some(95.);
        mutate(&mut changed);
        assert_eq!(
            f.call(
                "/api/gse/config",
                serde_json::to_value(&changed).unwrap(),
                Some("cold-origin-token")
            )
            .await
            .0,
            StatusCode::OK,
            "{name}"
        );
        assert!(
            f.state.gse.lock().unwrap().turbo_evidence.is_none(),
            "{name}"
        );
        println!("COLD_FLOW_CONTROL prior_config_field={name} evidence=CLEARED");
    }
    for (name, mutate) in [
        (
            "pressure_capture_epoch",
            (|rt: &mut Runtime| rt.valve_pressure_epoch += 1) as fn(&mut Runtime),
        ),
        ("mass_capture_epoch", |rt: &mut Runtime| {
            rt.mass_source_epoch += 1
        }),
    ] {
        install_evidence();
        mutate(&mut f.state.gse.lock().unwrap());
        assert_eq!(
            f.call(
                "/api/gse/config",
                serde_json::to_value(&inert).unwrap(),
                Some("cold-origin-token")
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(
            f.state.gse.lock().unwrap().turbo_evidence.is_none(),
            "{name}"
        );
        println!("COLD_FLOW_CONTROL changed_context={name} evidence=CLEARED");
    }
    for name in ["fill_targets", "thermal_calibration"] {
        install_evidence();
        if name == "fill_targets" {
            f.state.fill_targets.lock().unwrap().nitrous.target_mass_kg += 1.;
        } else {
            f.state.loadcell_calibration.lock().unwrap().thermal.insert(
                "KG50".into(),
                crate::loadcell::ThermalCalibration {
                    reference_c: 20.,
                    raw_per_c: 0.01,
                    points: vec![],
                },
            );
        }
        assert_eq!(
            f.call(
                "/api/gse/config",
                serde_json::to_value(&inert).unwrap(),
                Some("cold-origin-token")
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(
            f.state.gse.lock().unwrap().turbo_evidence.is_none(),
            "{name}"
        );
        println!("COLD_FLOW_CONTROL changed_context={name} evidence=CLEARED");
    }
    f.state.gse.lock().unwrap().engine.config = base_config.clone();
    let clock = f.state.launch_clock_snapshot();
    let flight = f.state.local_flight_state_snapshot();
    let key = f.start().await;
    let mut retarget = base_config;
    retarget.cold_flow_target_psi = Some(95.);
    assert_eq!(
        f.call(
            "/api/gse/config",
            serde_json::to_value(&retarget).unwrap(),
            Some("cold-origin-token")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.state
            .gse
            .lock()
            .unwrap()
            .engine
            .config
            .cold_flow_target_psi,
        Some(100.)
    );
    assert!(!f.state.gse.lock().unwrap().calibration_import_allowed());
    assert_eq!(
        f.call(
            "/api/gse/qualification/reset",
            serde_json::json!({}),
            Some("cold-origin-token")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let early = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    assert!(!early.awaiting_at_edge);
    f.hold();
    let envelope = f.state.sequence_progress.lock().unwrap().envelope(
        automatic_generation(&f.state),
        crate::sequence_progress_service::now_ms(),
    );
    let active = envelope
        .active
        .iter()
        .find(|s| s.key == key)
        .expect("authoritative current cold run");
    assert_eq!(active.kind, Kind::ColdFlow);
    assert!(active.cold_flow_release_allowed);
    assert!(active.cancel_allowed);
    assert!(
        active
            .steps
            .iter()
            .flat_map(|s| s.watch.iter())
            .any(|w| w.unit.as_deref() == Some("psi"))
    );
    f.edge(early).await;
    assert_eq!(f.phase(), ColdPhase::AwaitingRelease);
    assert_eq!(f.actual, CLOSED);
    assert!(!crate::gse::pilot_report_can_advance_launch_clock(&f.state));
    let blocked = vec![TelemetryCommand::Launch, TelemetryCommand::Igniter];
    #[cfg(any(feature = "hitl_mode", feature = "test_fire_mode"))]
    let blocked = {
        let mut values = blocked;
        values.push(TelemetryCommand::GroundStationLaunch);
        values
    };
    #[cfg(feature = "hitl_mode")]
    let blocked = {
        let mut values = blocked;
        values.push(TelemetryCommand::IgniterSequence);
        values
    };
    for cmd in blocked {
        assert_eq!(crate::gse::command_allowed(&f.state, &cmd), Some(false));
        assert!(crate::gse::handle_command(&f.state, &cmd));
    }
    let ticket = LaunchAdmission::capture(&f.state);
    // Isolate the existing dispatch-layer ColdFlow exclusion; mandatory
    // ignition-pair admission is exercised through actual SDK peers separately.
    assert_eq!(
        with_launch_admission(&f.state, ticket, false, || panic!(
            "rocket Launch admitted during cold flow"
        )).unwrap_err(),
        "Rocket Launch is inhibited while cold flow owns the valves"
    );
    let ready = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    assert!(ready.awaiting_at_edge);
    f.edge(ready.clone()).await;
    f.until(ColdPhase::Flowing, 100.);
    let before = f.sent.lock().unwrap().len();
    f.edge(ready).await;
    f.pump();
    assert_eq!(f.sent.lock().unwrap().len(), before);
    assert!(f.actual[0]);
    assert!(!f.actual[3] && !f.actual[4]);
    assert_eq!(f.state.launch_clock_snapshot(), clock);
    assert_eq!(f.state.local_flight_state_snapshot(), flight);
    // Actual source ingress must not turn either Sequence or PILOT reports into a clock.
    for (command, nonce) in [
        (ValveBoardCommands::Sequence as u8, 300),
        (ValveBoardCommands::PilotOpen as u8, 301),
    ] {
        let packet = sedsnet::packet::Packet::new_with_nonce(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "VB",
            crate::telemetry_task::get_current_timestamp_ms(),
            nonce,
            Arc::from([command, 1]),
        )
        .unwrap();
        crate::telemetry_task::apply_umbilical_status_at_ingress(&f.state, &packet);
        assert_eq!(f.state.launch_clock_snapshot(), clock);
        assert_eq!(f.state.local_flight_state_snapshot(), flight);
    }
    println!(
        "COLD_FLOW_CONTROL actual_sdk_flow=PASS physical_edge_current_run=PASS no_igniter_fc_launch_or_clock=PASS"
    );
    // Cancel by another authorized operator, with N2 report missing: all three
    // closes are issued, but no new dump/vent open or false completion.
    write_users(&f.users, true);
    let before = f.sent.lock().unwrap().len();
    assert_eq!(
        f.call(
            "/api/gse/sequences/cancel",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    f.pump();
    let tail = f.sent.lock().unwrap()[before..].to_vec();
    for (kind, code) in [
        ("ACTUATOR_COMMAND", 12),
        ("ACTUATOR_COMMAND", 13),
        ("VALVE_COMMAND", 3),
    ] {
        assert!(tail.iter().any(|(k, p)| k == kind && (p[0] & 0x7f) == code));
    }
    {
        let mut rt = f.state.gse.lock().unwrap();
        rt.test_now_ms = Some(rt.test_now_ms.unwrap() + 10);
        rt.valves[3] = None;
        rt.observe_valve(KEYS[4], false);
        rt.observe_valve(KEYS[0], false);
        rt.observe_pressure(Some(100.));
    }
    crate::gse::tick(&f.state, true);
    f.pump();
    assert_eq!(f.phase(), ColdPhase::CancelClosing);
    assert!(
        !f.sent.lock().unwrap()[before..]
            .iter()
            .any(|(k, p)| k == "VALVE_COMMAND" && matches!(p[0] & 0x7f, 1 | 2))
    );
    f.until(ColdPhase::Cancelled, 100.);
    let result = f
        .state
        .sequence_progress
        .lock()
        .unwrap()
        .result(&key)
        .unwrap();
    assert_eq!(result.outcome, crate::sequence_progress::Outcome::Cancelled);
    assert!(result.notification_id.is_some());
    assert_eq!(f.actual, RELIEVED);
    assert_eq!(f.state.local_flight_state_snapshot(), flight);
    println!(
        "COLD_FLOW_CONTROL revoked_origin_other_actor_cancel=PASS missing_n2_still_closes_other_outputs=PASS no_relief_until_fresh_proof=PASS"
    );
    // Revoke/expire origin AFTER physical edge capture: refusal has no normal Launch fallback.
    let mut f = Fixture::new().await;
    let key = f.start().await;
    f.hold();
    let edge = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    let before = f.sent.lock().unwrap().len();
    write_users(&f.users, true);
    f.edge(edge.clone()).await;
    f.pump();
    assert_eq!(f.phase(), ColdPhase::AwaitingRelease);
    assert_eq!(f.sent.lock().unwrap().len(), before);
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/release",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    f.until(ColdPhase::Flowing, 100.);
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/release",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/cancel",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    f.until(ColdPhase::Cancelled, 100.);
    f.state.clear_runtime_data_for_sim_reset();
    f.edge(edge).await;
    f.pump();
    assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
    println!(
        "COLD_FLOW_CONTROL revoked_edge_no_fallback=PASS current_modal_actor_release=PASS duplicate_release=REFUSED stale_reset_edge=REFUSED"
    );
    // Logout/expiry before the request's authorization refuses the captured edge.
    let mut f = Fixture::new().await;
    let key = f.start().await;
    f.hold();
    let edge = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    let before = f.sent.lock().unwrap().len();
    f.state
        .auth
        .logout(&f.state.auth_db, "cold-origin-token")
        .await
        .unwrap();
    f.edge(edge).await;
    f.pump();
    assert_eq!(f.sent.lock().unwrap().len(), before);
    assert_eq!(f.phase(), ColdPhase::AwaitingRelease);
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/cancel",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::OK
    );
    f.until(ColdPhase::Cancelled, 100.);
    let mut f = Fixture::new().await;
    f.start().await;
    f.hold();
    let edge = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    let before = f.sent.lock().unwrap().len();
    sqlx::query("UPDATE auth_sessions SET expires_at_ms=1 WHERE token='cold-origin-token'")
        .execute(&f.state.auth_db)
        .await
        .unwrap();
    f.edge(edge).await;
    f.pump();
    assert_eq!(f.sent.lock().unwrap().len(), before);
    assert_eq!(f.phase(), ColdPhase::AwaitingRelease);
    println!(
        "COLD_FLOW_CONTROL logout_or_expiry_before_authorization=REFUSED other_operator_safe_cancel=PASS persisted_independent_target=PASS"
    );

    // Pending Manual Stop has its original owner and never becomes Cold Flow or Launch.
    let mut f = Fixture::new().await;
    let key = f.start().await;
    f.hold();
    let edge = panel_edge(&f.state, automatic_generation(&f.state)).unwrap();
    let before = f.sent.lock().unwrap().len();
    f.state.manual_abort_intent.request_untimed();
    f.edge(edge).await;
    f.pump();
    assert_eq!(f.sent.lock().unwrap().len(), before);
    assert!(f.state.manual_abort_intent.pending_epoch().is_some());
    assert_eq!(
        f.call(
            "/api/gse/cold-flow/release",
            serde_json::to_value(&key).unwrap(),
            Some("cold-other-token")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );

    // Original epoch is captured BEFORE the actual authentication DB await.
    let mut f = Fixture::new().await;
    let key = f.start().await;
    f.hold();
    let before = f.sent.lock().unwrap().len();
    let mut held = Vec::new();
    while let Ok(Ok(c)) = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        f.state.auth_db.acquire(),
    )
    .await
    {
        held.push(c);
    }
    assert!(!held.is_empty());
    let router = crate::gse::routes().with_state(f.state.clone());
    let request = HttpRequest::builder()
        .method("POST")
        .uri("/api/gse/cold-flow/release")
        .header("content-type", "application/json")
        .header("authorization", "Bearer cold-origin-token")
        .body(Body::from(serde_json::to_vec(&key).unwrap()))
        .unwrap();
    let mut waiting = Box::pin(router.oneshot(request));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    let original = automatic_generation(&f.state);
    super::super::reset_generation::invalidate_capture_epoch(&f.state, original).unwrap();
    drop(held);
    assert_eq!(waiting.await.unwrap().status(), StatusCode::CONFLICT);
    f.pump();
    assert_eq!(f.sent.lock().unwrap().len(), before);
    println!(
        "COLD_FLOW_CONTROL retained_manual_stop=PASS original_epoch_across_real_auth_await=REFUSED no_new_sdk_owner=PASS"
    );

    // Dedicated policy: unexpected nitrous parks/closes; pressure ceiling
    // requests cancellation without inventing an automatic Abort.
    let mut f = Fixture::new().await;
    f.start().await;
    f.until(ColdPhase::Pressurizing, 0.);
    f.actual[4] = true;
    {
        let mut rt = f.state.gse.lock().unwrap();
        let now = rt.test_now_ms.unwrap() + 1;
        rt.test_now_ms = Some(now);
        rt.observe_valve(KEYS[4], true);
    }
    crate::gse::report_safety(&f.state);
    assert_ne!(
        f.state.gse.lock().unwrap().engine.status.phase,
        Phase::Fault
    );
    f.step(0.);
    assert_eq!(f.phase(), ColdPhase::PauseClosing);
    assert_eq!(
        f.state
            .gse
            .lock()
            .unwrap()
            .cold_flow
            .as_ref()
            .unwrap()
            .controller
            .observation()
            .issue,
        Some(gse_sequence::cold_flow::Issue::UnexpectedNitrous)
    );
    assert!(
        !f.state
            .gse
            .lock()
            .unwrap()
            .cold_flow
            .as_ref()
            .unwrap()
            .controller
            .paused_ready()
    );
    assert_eq!(f.state.manual_abort_intent.epoch(), 0);
    let mut f = Fixture::new().await;
    f.start().await;
    f.until(ColdPhase::Pressurizing, 0.);
    f.step(200.);
    assert_eq!(f.phase(), ColdPhase::CancelClosing);
    f.until(ColdPhase::Cancelled, 200.);
    assert_eq!(f.state.manual_abort_intent.epoch(), 0);
    assert_eq!(f.state.local_flight_state_snapshot(), FlightState::Idle);
    println!(
        "COLD_FLOW_CONTROL unexpected_nitrous=ParkedUnverified pressure_ceiling=safe_cancel no_new_auto_abort=PASS"
    );
}

/// Only the independently named owned worker pauses. Normal controller calls
/// and all inherited fixtures take no synthetic admission result.
pub(super) fn before_packet_admission(state: &AppState, packet: &sedsnet::packet::Packet) {
    if std::thread::current().name() == Some("coldflow-packet-attempt") {
        super::super::nonwaiting_admission_tests::after_prepared_packet(state, packet);
    }
}

#[tokio::test]
async fn actual_cold_flow_held_core_preserves_run_packet_and_stop_revokes() {
    const NAME: &str = "gse::cold_flow::tests::actual_cold_flow_held_core_preserves_run_packet_and_stop_revokes";
    const CHILD: &str = "GS_COLD_FLOW_CORE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1").env("GS_RRLS_ROLE", "off").env("GS_SIM_MODE", "0")
            .env_remove("GS_SIM_CLOCK_FILE")
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(45);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline { let _ = child.kill(); let _ = child.wait(); panic!("owned ColdFlow core control timed out"); }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "{text}"); assert!(text.contains("1 passed"));
        println!("{text}"); return;
    }
    fn holder(state: &Arc<AppState>) -> std::thread::JoinHandle<sedsnet::TelemetryResult<()>> {
        let router = state.topology_router.get().unwrap().clone();
        let seed = sedsnet::packet::Packet::new(crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")], "GS", 100, Arc::from([0u8])).unwrap();
        std::thread::Builder::new().name("production-sdk-core-holder".into()).spawn(move || router.seed_managed_variable(seed)).unwrap()
    }
    // Actual public SDK's clock callback holds its core, not a mock queue result.
    let mut f = Fixture::configured(true).await;
    let key = f.start().await; f.hold();
    let original = { let rt=f.state.gse.lock().unwrap(); let s=rt.cold_flow.as_ref().unwrap(); (s.key.clone(),s.revision,s.controller.observation()) };
    let pause = super::super::nonwaiting_admission_tests::Pause::install(&f.state);
    let held = holder(&f.state); pause.core_entered.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let (tx,rx)=std::sync::mpsc::channel();let state=f.state.clone();let request=key.clone();
    let worker=std::thread::spawn(move || {
        assert!(tick(&state,true));
        let result=perform(&state,&request,LaunchAdmission::capture(&state),Request::Cancel);
        // A different actual DGate consumer must also complete before SDK release.
        { let _gate=recording_publication_gate(); }
        tx.send(result).unwrap();
    });
    let result=rx.recv_timeout(std::time::Duration::from_millis(500)).expect("ColdFlow tick/cancel and unrelated DGate complete while SDK core remains held");
    assert!(result.unwrap_err().contains("busy")); worker.join().unwrap();
    { let rt=f.state.gse.lock().unwrap();let s=rt.cold_flow.as_ref().unwrap();assert_eq!((s.key.clone(),s.revision,s.controller.observation()),original);assert!(!s.retired); }
    pause.release();assert!(held.join().unwrap().is_ok());drop(pause);
    println!("COLD_FLOW_CORE held_sdk_tick_cancel_dispatch_completed_before_release=true");
    // Pause after real packet construction, then acquire the actual SDK core.
    // Exact retained PacketID/nonce/time must survive; a new Stop must revoke it.
    let selected = std::env::var("GS_COLD_FLOW_FLOOR_CASE").ok();
    for scenario in ["resume", "manual_stop", "sdk_abort", "stale_evidence", "backward_capture", "equal_capture"] {
        if selected.as_ref().is_some_and(|selected| selected != scenario) { continue; }
        let mut f=Fixture::configured(true).await;let _key=f.start().await;
        // Select the genuine last re-isolation close immediately before the
        // source opening; no Effects or source timestamp is synthesized.
        let mut pilot_floor = 0;
        for _ in 0..20 {
            let observation=f.state.gse.lock().unwrap().cold_flow.as_ref().unwrap().controller.observation();
            if observation.phase==ColdPhase::Pressurizing && observation.waiting==Some(0) {
                pilot_floor = observation.command_since_ms;
            }
            if observation.phase==ColdPhase::Pressurizing && observation.waiting==Some(1)
                && observation.pending_mask & (1<<3)!=0 { break; }
            f.step(0.);
        }
        { let o=f.state.gse.lock().unwrap().cold_flow.as_ref().unwrap().controller.observation();
          assert_eq!(o.phase,ColdPhase::Pressurizing);assert_eq!(o.waiting,Some(1));assert_ne!(o.pending_mask&(1<<3),0); }
        f.pump();
        { let mut rt=f.state.gse.lock().unwrap(); let now=rt.test_now_ms.unwrap()+10;
          rt.test_now_ms=Some(now);rt.observe_pressure(Some(0.));
          for (i,key) in KEYS.into_iter().enumerate(){rt.observe_valve(key,f.actual[i]);} }
        assert!(pilot_floor > 0, "original PILOT close command floor observed from actual controller");
        const SOURCE_UTC:i64 = 1_793_563_201_000;
        if matches!(scenario,"backward_capture"|"equal_capture") {
            let mut rt=f.state.gse.lock().unwrap();let now=rt.now();
            assert!(now>pilot_floor);
            let receipt=super::super::ValveReceipt { network_ms:SOURCE_UTC,monotonic_at:Instant::now(),simulation_ms:Some(now) };
            assert_eq!(rt.observe_valve_source(KEYS[0],false,SOURCE_UTC,900001,receipt),Some(0));
            assert_eq!(rt.valves[0].unwrap().at_ms,now);
        }
        let before=f.packets.lock().unwrap().len();
        let pause=super::super::nonwaiting_admission_tests::Pause::install(&f.state);
        let state=f.state.clone();let (tx,rx)=std::sync::mpsc::channel();
        let worker=std::thread::Builder::new().name("coldflow-packet-attempt".into()).spawn(move || {
            tx.send(tick(&state,true)).unwrap();
        }).unwrap();
        pause.prepared.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let packet=pause.packet.lock().unwrap().as_ref().unwrap().clone();
        assert_eq!(packet.data_type(),crate::telemetry_schema::data_type("ACTUATOR_COMMAND"));
        assert_eq!(packet.payload(),&[OPEN[3]|0x80],"the retained effect is the actual N2 opening");
        let expected=(packet.packet_id(),packet.nonce(),packet.timestamp());
        let held=holder(&f.state);pause.core_entered.recv_timeout(std::time::Duration::from_secs(2)).unwrap();pause.proceed.send(()).unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_millis(500)).expect("constructed Busy plan returns while SDK held"));worker.join().unwrap();
        { let rt=f.state.gse.lock().unwrap();let p=&rt.cold_flow.as_ref().unwrap().pending.as_ref().unwrap().plan;let actual=p.packet.as_ref().unwrap();assert_eq!((actual.packet_id(),actual.nonce(),actual.timestamp()),expected);assert_eq!(p.next_effect,0); }
        if scenario == "manual_stop" { f.state.manual_abort_intent.request(); }
        pause.release();assert!(held.join().unwrap().is_ok());drop(pause);
        if scenario == "sdk_abort" {
            let router=f.state.topology_router.get().unwrap();
            crate::telemetry_task::queue_manual_abort_wire(router,crate::manual_abort_intent::WirePolicy::Untimed).unwrap();
            assert!(router.try_has_pending_reliable_tx_type(crate::telemetry_schema::types::ABORT).unwrap());
        }
        if matches!(scenario,"backward_capture"|"equal_capture") {
            let mut rt=f.state.gse.lock().unwrap();let now=rt.now();
            let capture = if scenario=="equal_capture" {pilot_floor} else {pilot_floor-1};
            let newer_utc=SOURCE_UTC+1;
            let network_ms=newer_utc+i64::try_from(now-capture).unwrap();
            assert!(network_ms>SOURCE_UTC && newer_utc>SOURCE_UTC);
            let receipt=super::super::ValveReceipt {network_ms,monotonic_at:Instant::now(),simulation_ms:Some(now)};
            let age=rt.observe_valve_source(KEYS[0],false,newer_utc,900002,receipt).expect("newer UTC is genuinely accepted by actual Runtime ingestion");
            assert_eq!(age,now-capture);assert!(age<2000);
            assert_eq!(rt.valves[0].unwrap().at_ms,capture);
            assert_eq!(rt.last_valve_captures[0].source_ms,newer_utc);
            assert!(capture<=pilot_floor);
            println!("COLD_FLOW_FLOOR scenario={scenario} original_since={pilot_floor} accepted_newer_utc={newer_utc} captured_at={capture} now={now} age={age} original_packet={expected:?}");
        }
        if scenario == "stale_evidence" {
            let mut rt=f.state.gse.lock().unwrap();rt.test_now_ms=Some(rt.test_now_ms.unwrap()+2200);
        }
        tick(&f.state,true);f.pump();
        let delivered=f.packets.lock().unwrap();
        let matching=delivered[before..].iter().filter(|p| (p.packet_id(),p.nonce(),p.timestamp())==expected).count();
        assert_eq!(matching,usize::from(scenario=="resume"),"exact original packet outcome {scenario}");
        if matches!(scenario,"stale_evidence"|"backward_capture"|"equal_capture") {
            assert!(delivered[before..].iter().all(|p| CLOSE.into_iter().any(|c| p.payload()==[c|0x80])),"only closure requests follow stale evidence");
            let rt=f.state.gse.lock().unwrap();let controller=&rt.cold_flow.as_ref().unwrap().controller;
            assert_eq!(controller.observation().phase,ColdPhase::PauseClosing);assert!(!controller.paused_ready());
        } else { assert_eq!(delivered.len()-before,usize::from(scenario=="resume"),"no extra or substituted outputs"); }
        drop(delivered);
        if matches!(scenario,"manual_stop"|"sdk_abort") { assert!(f.state.gse.lock().unwrap().cold_flow.as_ref().unwrap().retired); }
        else if scenario=="resume" { tick(&f.state,true);f.pump();assert_eq!(f.packets.lock().unwrap().len()-before,1,"successful original effect never republished"); }
        println!("COLD_FLOW_CORE original_packet_preserved=true scenario={scenario} delivered={matching}");
    }
    // A manual PILOT release is different: a raced Busy response must restore
    // only its own unadmitted AwaitingRelease state, never create an auto-open.
    let mut f=Fixture::configured(true).await;let key=f.start().await;f.hold();
    let before=f.packets.lock().unwrap().len();
    let pause=super::super::nonwaiting_admission_tests::Pause::install(&f.state);
    let state=f.state.clone();let requested=key.clone();let(tx,rx)=std::sync::mpsc::channel();
    let worker=std::thread::Builder::new().name("coldflow-packet-attempt".into()).spawn(move||{
        tx.send(perform(&state,&requested,LaunchAdmission::capture(&state),Request::Release)).unwrap();
    }).unwrap();
    pause.prepared.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let refused=pause.packet.lock().unwrap().as_ref().unwrap().packet_id();
    let held=holder(&f.state);pause.core_entered.recv_timeout(std::time::Duration::from_secs(2)).unwrap();pause.proceed.send(()).unwrap();
    assert!(rx.recv_timeout(std::time::Duration::from_millis(500)).expect("manual release refuses Busy before holder release").is_err());worker.join().unwrap();
    assert_eq!(f.phase(),ColdPhase::AwaitingRelease);
    assert!(f.state.gse.lock().unwrap().cold_flow.as_ref().unwrap().pending.is_none());
    write_users(&f.users,true);
    pause.release();assert!(held.join().unwrap().is_ok());drop(pause);
    tick(&f.state,true);f.pump();assert_eq!(f.packets.lock().unwrap().len(),before);
    let response=f.call("/api/gse/cold-flow/release",serde_json::to_value(&key).unwrap(),Some("cold-origin-token")).await;
    assert!(matches!(response.0,StatusCode::FORBIDDEN|StatusCode::UNAUTHORIZED));
    assert_eq!(f.packets.lock().unwrap().len(),before);
    let response=f.call("/api/gse/cold-flow/release",serde_json::to_value(&key).unwrap(),Some("cold-other-token")).await;
    assert_eq!(response.0,StatusCode::OK,"{response:?}");f.pump();
    let packets=f.packets.lock().unwrap();assert_eq!(packets.len(),before+1);
    assert!(packets.iter().all(|p|p.packet_id()!=refused));
    assert_eq!(packets.last().unwrap().payload(),&[OPEN[0]|0x80]);
    assert!(std::mem::size_of::<Controller>()<=256,"release snapshot must stay bounded primitive state");
    println!("COLD_FLOW_CORE release_busy_no_automatic_pilot=true revoked_actor_refused=true fresh_other_actor_release=true controller_bytes={}",std::mem::size_of::<Controller>());
}

#[test]
fn cold_flow_only_temporary_sdk_owners_are_retryable() {
    use sedsnet::router::TryQueuedPacketError::*;
    for (error, expected) in [(CoreBusy,true),(SchemaBusy,true),(BlockedPendingReliableType,false),
        (Capacity,false),(StalePreparation,false),(AlreadyAdmitted,false),(OriginNotAllowed,false)] {
        assert_eq!(retryable_queue_error(error), expected, "{error:?}");
    }
}

/// Same actual route, DB actor and public SDK. Only the three outside-owner
/// pauses are synthetic; no SDK admission/recording/physical result is faked.
#[tokio::test]
async fn actual_cold_flow_preparation_busy_retains_origin_and_all_revocations() {
    const NAME: &str = "gse::cold_flow::tests::actual_cold_flow_preparation_busy_retains_origin_and_all_revocations";
    const CHILD: &str = "GS_COLD_FLOW_PREPARATION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("GS_RRLS_ROLE", "off")
            .env("GS_SIM_MODE", "0")
            .env_remove("GS_SIM_CLOCK_FILE")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(45);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded preparation fixture exceeded45s");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let result = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.status.success(), "{text}");
        assert!(text.contains("1 passed"));
        println!("{text}");
        return;
    }
    use crate::fill_recording::coldflow_test_hooks::{Guard, Stage};
    type Response = Option<(StatusCode, serde_json::Value)>;
    fn start_http(
        state: Arc<AppState>,
    ) -> (
        std::thread::JoinHandle<()>,
        tokio::task::AbortHandle,
        std::sync::mpsc::Receiver<Response>,
    ) {
        let (send, recv) = std::sync::mpsc::channel();
        let (abort_send, abort_recv) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let task = tokio::spawn(async move {
                        let request = HttpRequest::builder()
                            .method("POST")
                            .uri("/api/gse/cold-flow/start")
                            .header("content-type", "application/json")
                            .header("authorization", "Bearer cold-origin-token")
                            .body(Body::from("{}"))
                            .unwrap();
                        let response = crate::gse::routes()
                            .with_state(state)
                            .oneshot(request)
                            .await
                            .unwrap();
                        let status = response.status();
                        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                        (
                            status,
                            serde_json::from_slice(&bytes).unwrap_or_else(
                                |_| serde_json::json!({"text":String::from_utf8_lossy(&bytes)}),
                            ),
                        )
                    });
                    abort_send.send(task.abort_handle()).unwrap();
                    match task.await {
                        Ok(value) => Some(value),
                        Err(error) if error.is_cancelled() => None,
                        Err(error) => panic!("actual HTTP task failed: {error}"),
                    }
                });
            let _ = send.send(result);
        });
        (
            thread,
            abort_recv
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            recv,
        )
    }
    fn holder(state: &Arc<AppState>) -> std::thread::JoinHandle<sedsnet::TelemetryResult<()>> {
        let router = state.topology_router.get().unwrap().clone();
        let seed = sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("AV_BAY_UNDERGLOW"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            "GS",
            100,
            Arc::from([0u8]),
        )
        .unwrap();
        std::thread::Builder::new()
            .name("production-sdk-core-holder".into())
            .spawn(move || router.seed_managed_variable(seed))
            .unwrap()
    }
    fn file_count(f: &Fixture) -> usize {
        std::fs::read_dir(f._root.0.join("recordings"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|p| {
                p.file_name()
                    .to_string_lossy()
                    .starts_with("groundstation_recording_")
                    && p.path().extension().is_some_and(|e| e == "db")
            })
            .count()
    }
    for stage in [Stage::Initial, Stage::OriginCurrent, Stage::Final] {
        let mut f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, stage);
        let (http, _abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let core = super::super::nonwaiting_admission_tests::Pause::install(&f.state);
        let held = holder(&f.state);
        core.core_entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        pause.release();
        let early = reply.recv_timeout(std::time::Duration::from_millis(200));
        assert!(
            f.state.gse.lock().unwrap().cold_flow.is_none(),
            "no controller/session mutation while CoreBusy"
        );
        assert!(f.packets.lock().unwrap().is_empty());
        core.release();
        assert!(held.join().unwrap().is_ok());
        drop(core);
        let early_response = early.is_ok();
        let response = early
            .unwrap_or_else(|_| {
                reply
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
            })
            .unwrap();
        http.join().unwrap();
        assert!(
            !early_response,
            "same original preparation must await transientCoreBusy {stage:?}: {response:?}"
        );
        assert_eq!(response.0, StatusCode::OK, "{response:?}");
        let key: RunKey = serde_json::from_value(response.1["key"].clone()).unwrap();
        let (count, keys) = pause.keys();
        assert_eq!(count, 1, "one original reporting identity");
        assert_eq!(keys, vec![key.clone()]);
        assert_eq!(
            f.state.gse.lock().unwrap().cold_flow.as_ref().unwrap().key,
            key
        );
        assert_eq!(file_count(&f), 1);
        f.pump();
        let before: Vec<_> = f
            .packets
            .lock()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p.packet_id(),
                    p.nonce(),
                    p.timestamp(),
                    p.payload().to_vec(),
                )
            })
            .collect();
        assert!(!before.is_empty(), "actual initial close effects delivered");
        tick(&f.state, true);
        f.pump();
        let after: Vec<_> = f
            .packets
            .lock()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p.packet_id(),
                    p.nonce(),
                    p.timestamp(),
                    p.payload().to_vec(),
                )
            })
            .collect();
        assert_eq!(after, before, "original effects never replayed");
        println!(
            "COLD_FLOW_PREPARATION stage={stage:?} same_origin=true key_once=true file_once=true original_effects_once=true"
        );
    }
    for scenario in [
        "manual_stop",
        "permission",
        "session_revoke",
        "session_expired",
    ] {
        let f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, Stage::OriginCurrent);
        let (http, _abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        match scenario {
            "manual_stop" => {
                f.state.manual_abort_intent.request();
            }
            "permission" => write_users(&f.users, true),
            "session_revoke" => {
                sqlx::query("DELETE FROM auth_sessions WHERE token='cold-origin-token'")
                    .execute(&f.state.auth_db)
                    .await
                    .unwrap();
            }
            "session_expired" => {
                sqlx::query(
                    "UPDATE auth_sessions SET expires_at_ms=1 WHERE token='cold-origin-token'",
                )
                .execute(&f.state.auth_db)
                .await
                .unwrap();
            }
            _ => unreachable!(),
        }
        pause.release();
        let response = reply
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        http.join().unwrap();
        assert!(
            matches!(
                response.0,
                StatusCode::CONFLICT | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "{scenario}: {response:?}"
        );
        assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
        assert!(f.packets.lock().unwrap().is_empty());
        assert_eq!(pause.keys().0, 0);
        println!("COLD_FLOW_PREPARATION revoked={scenario} refused=true no_outputs=true");
    }
    // A real Pause invalidates the already-ready receipt; no automatic reopen.
    {
        let f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, Stage::Final);
        let (http, _abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        // The existing actor deliberately deletes empty archives on Pause.
        // Commit an explicitly named fixture marker through its actual queue
        // before asserting preservation of a nonempty closed recording.
        f.state
            .db_queue_tx
            .send(crate::telemetry_db::DbQueueItem::Write(
                crate::telemetry_db::DbWrite::Telemetry {
                    timestamp_ms: crate::telemetry_task::get_current_timestamp_ms() as i64,
                    source_timestamp_ms: None,
                    data_type: "COLD_FLOW_FIXTURE_MARKER".into(),
                    sender_id: "fixture".into(),
                    values_json: Some("[1]".into()),
                    payload_json: "[]".into(),
                },
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let rows: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM telemetry WHERE data_type='COLD_FLOW_FIXTURE_MARKER'",
                )
                .fetch_one(&f.state.telemetry_db_pool())
                .await
                .unwrap();
                if rows == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        f.state
            .db_queue_tx
            .send(crate::telemetry_db::DbQueueItem::Control(
                crate::telemetry_db::RecordingCommand::Pause,
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !matches!(
                f.state.recording_status_snapshot().mode,
                crate::telemetry_db::RecordingModeWire::Paused
            ) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        pause.release();
        let response = reply
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        http.join().unwrap();
        assert_eq!(response.0, StatusCode::CONFLICT);
        assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
        assert!(f.packets.lock().unwrap().is_empty());
        assert_eq!(file_count(&f), 1);
        assert_eq!(pause.keys().0, 1);
        println!("COLD_FLOW_PREPARATION revoked=recording_pause refused=true no_reopen=true");
    }
    // A concurrent new start cannot replace the retained original run/key.
    {
        let f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, Stage::Final);
        let (http, _abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let second = f
            .call(
                "/api/gse/cold-flow/start",
                serde_json::json!({}),
                Some("cold-origin-token"),
            )
            .await;
        assert_eq!(second.0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(second.1["reason"],"busy","existing preparation Busy contract");
        assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
        assert_eq!(pause.keys().0, 1);
        pause.release();
        let first = reply
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        http.join().unwrap();
        assert_eq!(first.0, StatusCode::OK);
        assert_eq!(pause.keys().0, 1);
        assert_eq!(file_count(&f), 1);
        println!("COLD_FLOW_PREPARATION concurrent_new_run=refused original_once=true");
    }
    // Genuine HTTP-task cancellation during a Busy wait retires its origin.
    {
        let mut f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, Stage::Final);
        let (http, abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let core = super::super::nonwaiting_admission_tests::Pause::install(&f.state);
        let held = holder(&f.state);
        core.core_entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        pause.release();
        assert!(
            reply
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        abort.abort();
        assert!(
            reply
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
                .is_none()
        );
        http.join().unwrap();
        core.release();
        assert!(held.join().unwrap().is_ok());
        drop(core);
        f.pump();
        assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
        assert!(f.packets.lock().unwrap().is_empty());
        println!("COLD_FLOW_PREPARATION disconnected=retired no_late_output=true");
    }
    // Use the REAL original five-second wall deadline. Consume500ms outside
    // owners first, so the existing5s SDK-holder fixture has margin to release.
    {
        let mut f = Fixture::configured(true).await;
        let pause = Guard::install(&f.state, Stage::Initial);
        let started = Instant::now();
        let (http, _abort, reply) = start_http(f.state.clone());
        pause
            .entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let core = super::super::nonwaiting_admission_tests::Pause::install(&f.state);
        let held = holder(&f.state);
        core.core_entered
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        pause.release();
        let response = reply
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .unwrap();
        core.release();
        assert!(held.join().unwrap().is_ok());
        drop(core);
        http.join().unwrap();
        assert_eq!(response.0, StatusCode::CONFLICT, "{response:?}");
        assert_eq!(response.1["reason"], "stale");
        assert!(started.elapsed() >= std::time::Duration::from_secs(5));
        assert!(f.state.gse.lock().unwrap().cold_flow.is_none());
        f.pump();
        assert!(f.packets.lock().unwrap().is_empty());
        assert_eq!(pause.keys().0, 0);
        println!("COLD_FLOW_PREPARATION original5s_timeout=refused no_renewal=true");
    }
}
