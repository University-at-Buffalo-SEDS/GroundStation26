use super::*;
use crate::auth::{CalibrationAccess, CommandAccess, Permissions, StreamRole};
use crate::runtime_mode::test_support::thread_mode;
use crate::station_mode::Mode;

/// An FC BOARD_HEALTH with word 14 carrying `igt` in bits 4..15.
fn health(igt: u32) -> [u32; 24] {
    let mut words = [0u32; 24];
    words[0] = 1;
    words[1] = 2 | 16 | 128;
    words[4] = 1000;
    words[14] = (TAG << 16) | (igt << 4) | 0b0101;
    words
}

/// Word 14 bits 4..15 (shifted down by 4).
fn bits(mode: u32, live: bool, gps: bool, drogue: bool, main: bool, reason: u32) -> u32 {
    mode | 1 << 3 | u32::from(live) << 4 | u32::from(gps) << 5 | u32::from(drogue) << 6 | u32::from(main) << 7
        | reason << 8
}

fn report(mode: FcMode) -> Report {
    Report { supported: true, mode, live_pyro: false, gps_simulated: false, drogue_dry_run: false, main_dry_run: false, reason: 0 }
}

fn principal(admin: bool, allowed: &[&str]) -> AuthPrincipal {
    AuthPrincipal {
        roles: if admin { vec![StreamRole::Admin] } else { Vec::new() },
        username: Some(if admin { "admin" } else { "operator" }.into()),
        permissions: Permissions { view_data: true, send_commands: true, ..Default::default() },
        expires_at_ms: None,
        anonymous: false,
        session_type: Some("session".into()),
        command_access: CommandAccess { allowed_commands: allowed.iter().map(|s| s.to_string()).collect() },
        calibration_access: CalibrationAccess::default(),
    }
}

fn observe(state: &AppState, report: Report) {
    state.igt.observe(report, Instant::now(), 1);
}

#[test]
fn word_14_decodes_every_igt_field() {
    let r = decode(&health(bits(2, true, false, true, false, 0))).unwrap();
    assert_eq!(r, Report { supported: true, mode: FcMode::Ready, live_pyro: true, gps_simulated: false, drogue_dry_run: true, main_dry_run: false, reason: 0 });
    let r = decode(&health(bits(5, false, true, false, true, 9))).unwrap();
    assert_eq!((r.mode, r.gps_simulated, r.main_dry_run, r.reason), (FcMode::Fault, true, true, 9));
    assert_eq!(reason_text(9), "unused reason code 9");
    for (wire, mode) in [(0, FcMode::Off), (1, FcMode::Validating), (3, FcMode::Sim), (4, FcMode::Done)] {
        assert_eq!(decode(&health(bits(wire, false, false, false, false, 0))).unwrap().mode, mode);
    }
    // Bit 7 clear: a flight image. Every other bit is ignored.
    let flight = decode(&health(2 | 1 << 4)).unwrap();
    assert!(!flight.supported);
    assert_eq!(flight.mode, FcMode::Off);
    assert!(!flight.live_pyro);
    // Untagged word 14 (older firmware or another board): nothing.
    let mut words = health(bits(2, false, false, false, false, 0));
    words[14] &= 0xFFFF;
    assert!(decode(&words).is_none());
    let mut words = health(bits(2, false, false, false, false, 0));
    words[1] |= 8;
    assert!(decode(&words).is_none());
    assert!(decode(&health(bits(6, false, false, false, false, 0))).is_none(), "unknown mode");
    // The cosmetic output bits 0..3 still decode alongside.
    assert_eq!(
        crate::network_variables::fc_outputs_from_health(&health(bits(3, false, false, false, false, 0))),
        [Some(true), None]
    );
}

#[tokio::test]
async fn status_json_has_exactly_the_contract_fields() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let before = serde_json::to_value(state.igt.status()).unwrap();
    assert_eq!(before["fc_mode"], "off");
    assert_eq!(before["supported"], false);
    assert!(before["updated_ms"].is_null());
    state.igt.observe(decode(&health(bits(3, false, true, false, false, 10))).unwrap(), Instant::now(), 1234);
    let json = serde_json::to_value(state.igt.status()).unwrap();
    let keys: std::collections::BTreeSet<_> = json.as_object().unwrap().keys().cloned().collect();
    let expected: std::collections::BTreeSet<String> = [
        "supported", "fc_mode", "live_pyro", "gps_simulated", "drogue_dry_run", "main_dry_run", "reason",
        "reason_text", "station_mode_igt", "updated_ms", "fc_image", "launch_latched", "launch_latch_reason",
        "image_notice", "igt_active", "mock_wet_dress", "hitl_replay", "sd_ready", "sd_retry_pending",
    ]
    .map(str::to_owned)
    .into();
    assert_eq!(keys, expected);
    assert_eq!(json["fc_mode"], "sim");
    assert_eq!(json["gps_simulated"], true);
    assert_eq!(json["reason"], 10);
    assert_eq!(json["reason_text"], "SD ring underrun (last value held)");
    assert_eq!(json["station_mode_igt"], true);
    assert_eq!(json["updated_ms"], 1234);
    assert!(state.igt.simulated());
    // A repeated identical report republishes nothing.
    assert!(state.igt.publish().is_some());
    state.igt.observe(decode(&health(bits(3, false, true, false, false, 10))).unwrap(), Instant::now(), 5000);
    assert!(state.igt.publish().is_none());
    assert_eq!(state.igt.status().updated_ms, Some(5000));
}

#[test]
fn transitions_raise_one_notice_each() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let text = |n: &Notice| match n {
        Notice::Info(t) | Notice::Warning(t) => t.clone(),
    };
    let off = report(FcMode::Off);
    assert!(transition_notices(None, off, true).is_empty(), "first report Off is quiet");
    let entered = transition_notices(Some(off), report(FcMode::Validating), true);
    assert!(text(&entered[0]).contains("entered"));
    let ready = transition_notices(Some(report(FcMode::Validating)), report(FcMode::Ready), true);
    assert!(text(&ready[0]).contains("Ready") && text(&ready[0]).contains("DRY-RUN"));
    let live = transition_notices(Some(report(FcMode::Ready)), Report { live_pyro: true, ..report(FcMode::Ready) }, true);
    assert!(matches!(&live[0], Notice::Warning(t) if t.contains("LIVE PYRO armed")));
    let sim = transition_notices(Some(report(FcMode::Ready)), Report { gps_simulated: true, ..report(FcMode::Sim) }, true);
    assert!(text(&sim[0]).contains("SIMULATED flight started"));
    let gps = transition_notices(
        Some(Report { gps_simulated: true, ..report(FcMode::Sim) }),
        report(FcMode::Done),
        true,
    );
    assert!(gps.iter().any(|n| text(n).contains("done")));
    assert!(gps.iter().any(|n| text(n) == "FC ground test: GPS switched to real."));
    let fault = transition_notices(Some(report(FcMode::Ready)), Report { reason: 8, ..report(FcMode::Fault) }, true);
    assert!(matches!(&fault[0], Notice::Warning(t) if t.contains("not stationary at Launch")));
    let reset = transition_notices(Some(report(FcMode::Sim)), off, true);
    assert!(matches!(&reset[0], Notice::Warning(t) if t.contains("left") && t.contains("reset")));
    // Outside an integrated ground test an FC in the mode is a warning only.
    let stray = transition_notices(Some(off), report(FcMode::Ready), false);
    assert_eq!(stray.len(), 1);
    assert!(matches!(&stray[0], Notice::Warning(t) if t.contains("while the station is in")));
    assert!(transition_notices(Some(off), off, false).is_empty());
}

#[tokio::test]
async fn simulated_follows_sim_and_done_after_a_replay_only() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Ready));
    assert!(!state.igt.simulated());
    observe(&state, report(FcMode::Sim));
    assert!(state.igt.simulated());
    observe(&state, report(FcMode::Done));
    assert!(state.igt.simulated(), "Done after a replay still describes the simulated flight");
    observe(&state, report(FcMode::Off));
    observe(&state, report(FcMode::Done));
    assert!(!state.igt.simulated(), "Done without a replay is not simulated");
}

#[tokio::test]
async fn ground_test_commands_exist_only_in_integrated_ground_test_mode() {
    for mode in [Mode::Normal, Mode::TestFire, Mode::Testing] {
        let _mode = thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        for name in IGT_COMMANDS_FOR_TEST {
            assert!(serde_json::from_value::<TelemetryCommand>(json!(name)).is_err(), "{mode:?} {name}");
            assert!(!crate::sequences::all_command_names().contains(&name), "{mode:?} {name}");
        }
        assert!(matches!(command_gate(&state, &TelemetryCommand::IgtDryRun), Some(Err(_))));
        assert!(!state.is_command_allowed(&TelemetryCommand::IgtExit));
        assert!(launch_refusal(&state).is_none(), "{mode:?} has no IGT Launch gate");
        let layout = crate::layout::load_layout().unwrap();
        assert!(layout.actions_tab.actions.iter().all(|a| !a.cmd.starts_with("Igt")), "{mode:?}");
        assert!(!serde_json::to_value(&layout).unwrap().as_object().unwrap().contains_key("igt_mode"));
        let policy = crate::sequences::default_action_policy();
        assert!(policy.controls.iter().all(|c| !c.cmd.starts_with("Igt")));
    }
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    for name in IGT_COMMANDS_FOR_TEST {
        let cmd: TelemetryCommand = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(crate::sequences::command_name(&cmd), name);
        assert_eq!(crate::sequences::control_section(name), Some(crate::sequences::ControlSection::GroundTest));
    }
    assert_eq!(&crate::sequences::all_command_names()[..4], &IGT_COMMANDS_FOR_TEST);
    assert!(!crate::sequences::all_command_names().contains(&"GroundStationLaunch"));
    assert!(serde_json::from_value::<TelemetryCommand>(json!("GroundStationLaunch")).is_err());
    assert!(serde_json::from_value::<TelemetryCommand>(json!("AdvanceFlightState")).is_err());
    let layout = crate::layout::load_layout().unwrap();
    let cmds: Vec<_> = layout.actions_tab.actions.iter().map(|a| a.cmd.as_str()).collect();
    assert_eq!(&cmds[..4], &IGT_COMMANDS_FOR_TEST);
    assert!(cmds.contains(&"Launch") && !cmds.contains(&"GroundStationLaunch"));
    assert!(layout.igt_mode && !layout.test_fire_mode);
    let boards = layout.network_tab.expected_boards.clone();
    for board in ["FC", "RF", "PB"] {
        assert!(boards.iter().any(|b| b == board), "IGT layout expects {board}");
    }
}

const IGT_COMMANDS_FOR_TEST: [&str; 4] = ["IgtEnter", "IgtLivePyro", "IgtDryRun", "IgtExit"];

#[test]
fn fc_command_bytes_follow_the_contract() {
    assert_eq!(fc_byte(&TelemetryCommand::IgtEnter), Some(29));
    assert_eq!(fc_byte(&TelemetryCommand::IgtLivePyro), Some(30));
    assert_eq!(fc_byte(&TelemetryCommand::IgtDryRun), Some(31));
    assert_eq!(fc_byte(&TelemetryCommand::IgtExit), Some(32));
    assert_eq!(crate::telemetry_task::hitl_flight_command_id(&TelemetryCommand::IgtExit), Some(32));
    assert!(requires_ticket(&TelemetryCommand::IgtEnter) && requires_ticket(&TelemetryCommand::IgtLivePyro));
    assert!(!requires_ticket(&TelemetryCommand::IgtDryRun) && !requires_ticket(&TelemetryCommand::IgtExit));
}

#[tokio::test]
async fn enter_is_gated_by_mode_state_and_fc_status() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let enter = TelemetryCommand::IgtEnter;
    let refused = |state: &AppState| command_gate(state, &enter).unwrap().unwrap_err();
    assert!(refused(&state).contains("No recent FC ground-test status"));
    observe(&state, Report::UNSUPPORTED);
    assert!(refused(&state).contains("no ground-test support"));
    observe(&state, report(FcMode::Off));
    assert!(refused(&state).contains("FC phase is unknown"));
    crate::fc_phase::observe(&state, 6, crate::fc_phase::Source::FcFlightState, 1);
    assert!(refused(&state).contains("only in Startup"));
    crate::fc_phase::observe(&state, 0, crate::fc_phase::Source::FcFlightState, 2);
    assert_eq!(command_gate(&state, &enter), Some(Ok(())));
    assert!(state.is_command_allowed(&enter));
    observe(&state, report(FcMode::Ready));
    assert!(refused(&state).contains("already Ready"));
    observe(&state, report(FcMode::Fault));
    assert_eq!(command_gate(&state, &enter), Some(Ok(())), "a fault may be retried");
    state.set_local_flight_state(FlightState::Armed);
    assert!(refused(&state).contains("before Armed"));
    state.set_local_flight_state(FlightState::Idle);
    state.set_launch_indicator_latched(true);
    assert!(refused(&state).contains("after a Launch"));
    state.set_launch_indicator_latched(false);
    // Dry-run and exit stay available.
    assert_eq!(command_gate(&state, &TelemetryCommand::IgtDryRun), Some(Ok(())));
    assert_eq!(command_gate(&state, &TelemetryCommand::IgtExit), Some(Ok(())));
    let _normal = thread_mode(Mode::Normal);
    assert!(refused(&state).contains("unavailable in Normal mode"));
}

#[tokio::test]
async fn live_pyro_needs_ready_before_launch() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let live = TelemetryCommand::IgtLivePyro;
    observe(&state, report(FcMode::Validating));
    assert!(command_gate(&state, &live).unwrap().unwrap_err().contains("Ready"));
    observe(&state, report(FcMode::Ready));
    assert_eq!(command_gate(&state, &live), Some(Ok(())));
    state.set_launch_indicator_latched(true);
    assert!(command_gate(&state, &live).unwrap().unwrap_err().contains("before Launch"));
}

#[tokio::test]
async fn plain_command_path_refuses_enter_and_live_pyro() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Ready));
    crate::fc_phase::observe(&state, 0, crate::fc_phase::Source::FcFlightState, 1);
    for cmd in [TelemetryCommand::IgtEnter, TelemetryCommand::IgtLivePyro] {
        let (status, message) = crate::web::dispatch_authorized_command(
            state.clone(), principal(true, &[]), cmd.clone(), 0, None, None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{cmd:?}: {message}");
        assert!(message.contains("/api/igt/prepare"), "{message}");
    }
    // The RRLS proxy path ends in the same dispatcher.
    let (status, _) = crate::web::dispatch_proxied_command(state.clone(), principal(true, &[]), TelemetryCommand::IgtLivePyro, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn live_pyro_review_needs_an_admin_and_the_typed_phrase() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Ready));
    let admin = principal(true, &[]);
    let operator = principal(false, &[]);
    let body = br#"{"action":"live_pyro"}"#;
    let (status, error) = prepare(&state, &operator, None, body).unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(error.contains("administrator"));
    let mut anonymous = principal(true, &[]);
    anonymous.anonymous = true;
    assert_eq!(prepare(&state, &anonymous, None, body).unwrap_err().0, StatusCode::FORBIDDEN);

    let ticket = prepare(&state, &admin, Some("s1".into()), body).unwrap();
    assert_eq!(ticket["required_confirmation"], "LIVE PYRO");
    assert!(ticket["expires_ms"].as_u64().unwrap() > 0);
    let id = ticket["ticket"].as_str().unwrap().to_owned();
    // Wrong phrase: refused, and the ticket is spent.
    let wrong = json!({"ticket": id, "confirmation": "live pyro"}).to_string();
    assert_eq!(confirm(&state, &admin, Some("s1".into()), wrong.as_bytes()).unwrap_err().0, StatusCode::BAD_REQUEST);
    let right = json!({"ticket": id, "confirmation": "LIVE PYRO"}).to_string();
    assert!(confirm(&state, &admin, Some("s1".into()), right.as_bytes()).unwrap_err().1.contains("already used"));

    // Another session cannot use the ticket.
    let id = prepare(&state, &admin, Some("s1".into()), body).unwrap()["ticket"].as_str().unwrap().to_owned();
    let right = json!({"ticket": id, "confirmation": "LIVE PYRO"}).to_string();
    assert!(confirm(&state, &admin, Some("s2".into()), right.as_bytes()).unwrap_err().1.contains("another session"));

    // Gates are checked again at confirm.
    let id = prepare(&state, &admin, Some("s1".into()), body).unwrap()["ticket"].as_str().unwrap().to_owned();
    observe(&state, report(FcMode::Sim));
    let right = json!({"ticket": id, "confirmation": "LIVE PYRO"}).to_string();
    assert_eq!(confirm(&state, &admin, Some("s1".into()), right.as_bytes()).unwrap_err().0, StatusCode::CONFLICT);

    observe(&state, report(FcMode::Ready));
    let id = prepare(&state, &admin, Some("s1".into()), body).unwrap()["ticket"].as_str().unwrap().to_owned();
    let right = json!({"ticket": id, "confirmation": " LIVE PYRO "}).to_string();
    assert!(matches!(confirm(&state, &admin, Some("s1".into()), right.as_bytes()), Ok(TelemetryCommand::IgtLivePyro)));
}

#[tokio::test]
async fn enter_review_needs_the_igt_enter_command_and_no_phrase() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Off));
    crate::fc_phase::observe(&state, 0, crate::fc_phase::Source::FcFlightState, 1);
    let body = br#"{"action":"enter"}"#;
    let restricted = principal(false, &["Launch"]);
    assert_eq!(prepare(&state, &restricted, None, body).unwrap_err().0, StatusCode::FORBIDDEN);
    let operator = principal(false, &["IgtEnter"]);
    let ticket = prepare(&state, &operator, None, body).unwrap();
    assert!(ticket["required_confirmation"].is_null());
    let confirmation = json!({"ticket": ticket["ticket"]}).to_string();
    assert!(matches!(confirm(&state, &operator, None, confirmation.as_bytes()), Ok(TelemetryCommand::IgtEnter)));
    // Not offered at all outside the mode.
    let _normal = thread_mode(Mode::Normal);
    assert_eq!(prepare(&state, &operator, None, body).unwrap_err().0, StatusCode::CONFLICT);
}

#[tokio::test]
async fn igt_launch_needs_fc_ground_test_ready_and_goes_to_the_fc() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    assert!(crate::telemetry_task::launch_goes_to_flight_computer());
    assert!(launch_refusal(&state).unwrap().contains("no recent FC ground-test status"));
    observe(&state, Report::UNSUPPORTED);
    assert!(launch_refusal(&state).unwrap().contains("no ground-test support"));
    observe(&state, Report { reason: 3, ..report(FcMode::Fault) });
    let refusal = launch_refusal(&state).unwrap();
    assert!(refusal.contains("Fault, not Ready") && refusal.contains("bad CSV header"), "{refusal}");
    observe(&state, report(FcMode::Ready));
    assert!(launch_refusal(&state).is_none());
    // The action policy shows the refusal on Launch.
    observe(&state, report(FcMode::Validating));
    let mut policy = crate::sequences::default_action_policy();
    for control in &mut policy.controls {
        control.enabled = true;
    }
    decorate_policy(&state, &mut policy);
    let launch = policy.controls.iter().find(|c| c.cmd == "Launch").unwrap();
    assert!(!launch.enabled && launch.disabled_reason.as_deref().unwrap().contains("Validating"));
    let dry = policy.controls.iter().find(|c| c.cmd == "IgtDryRun").unwrap();
    assert!(dry.enabled);
    let _tf = thread_mode(Mode::TestFire);
    assert!(!crate::telemetry_task::launch_goes_to_flight_computer());
}

#[test]
fn sim_samples_decode_to_separate_series() {
    let sample = |kind: f32, rest: [f32; 7]| {
        let mut v = vec![Some(kind)];
        v.extend(rest.map(Some));
        v
    };
    let imu = decode_sim_sample(&sample(1.0, [12.5, 0.1, 0.2, 9.8, 1.0, 2.0, 3.0])).unwrap();
    assert_eq!((imu.series, imu.t_s, imu.values.len()), ("SIM_IMU_DATA", 12.5, 6));
    assert_eq!(imu.values, vec![0.1, 0.2, 9.8, 1.0, 2.0, 3.0]);
    let baro = decode_sim_sample(&sample(2.0, [1.0, 812.0, 90000.0, 15.0, 0.0, 0.0, 0.0])).unwrap();
    assert_eq!((baro.series, baro.values.clone()), ("SIM_BAROMETER_DATA", vec![812.0, 90000.0, 15.0]));
    assert_eq!(decode_sim_sample(&sample(3.0, [0.0; 7])).unwrap().series, "SIM_GPS_DATA");
    assert_eq!(decode_sim_sample(&sample(4.0, [0.0; 7])).unwrap().values.len(), 6);
    let descent = decode_sim_sample(&sample(5.0, [0.0; 7])).unwrap();
    assert_eq!((descent.series, descent.values.len()), ("SIM_DESCENT_STATE", 4));
    for bad in [0.0, 6.0, 1.5, f32::NAN] {
        assert!(decode_sim_sample(&sample(bad, [0.0; 7])).is_none());
    }
    assert!(decode_sim_sample(&[Some(1.0); 6]).is_none());
    for kind in 1..=5 {
        let (series, _) = sim_series(kind).unwrap();
        assert!(series.starts_with("SIM_"), "never a real series name");
    }
}

#[tokio::test]
async fn sim_rows_come_only_from_the_fc_and_track_the_replayed_altitude() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let values: Vec<Option<f32>> = [2.0, 4.0, 640.0, 93000.0, 12.0, 0.0, 0.0, 0.0].map(Some).to_vec();
    assert!(sim_rows(&state, "RF", 1, &values).is_empty());
    let rows = sim_rows(&state, "FC", 1, &values);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].data_type, "SIM_BAROMETER_DATA");
    assert_eq!(rows[0].values, vec![Some(640.0), Some(93000.0), Some(12.0)]);
    assert_eq!(state.igt.sim_altitude_m(), Some(640.0));
}

#[tokio::test]
async fn estimator_notices_are_prefixed_while_the_fc_flies_replayed_data() {
    let notice = crate::fc_estimator::simulated_input(Notice::Warning("FC flight estimator diverged".into()));
    assert_eq!(notice, Notice::Warning("SIMULATED input: FC flight estimator diverged".into()));
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Sim));
    assert!(crate::mission_phase::flight_simulated(&state));
    let _normal = thread_mode(Mode::Normal);
    assert!(!crate::mission_phase::flight_simulated(&state), "only in an integrated ground test");
}

#[tokio::test]
async fn deployments_in_a_replayed_flight_are_labelled_simulated() {
    use crate::recovery_events::{Deployment, Evidence, notification_text_igt, notify};
    let dry = notification_text_igt(Deployment::Drogue, Evidence::Message { altitude_m: 812.4 }, 0, Some(20_100), false);
    assert!(dry.starts_with("SIMULATED flight: drogue DRY-RUN (not fired) at 812 m"), "{dry}");
    assert!(dry.contains("T+20.1 s"));
    let live = notification_text_igt(Deployment::Main, Evidence::Message { altitude_m: 240.0 }, 0, None, true);
    assert!(live.starts_with("SIMULATED flight: LIVE main fired at 240 m"), "{live}");

    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe(&state, report(FcMode::Sim));
    state.igt.note_sim_altitude(700.0);
    let text = notify(&state, Deployment::Drogue, Evidence::State { altitude_m: None }, 0).unwrap();
    assert!(text.starts_with("SIMULATED flight: drogue DRY-RUN (not fired) at 700 m (latest simulated telemetry)"), "{text}");
    observe(&state, Report { live_pyro: true, ..report(FcMode::Sim) });
    let text = crate::recovery_events::observe_fc_message(&state, "PR approx altitude: 250", 0).unwrap();
    assert!(text.starts_with("SIMULATED flight: LIVE main fired at 250 m"), "{text}");
    // Real flight (FC not replaying): the normal text.
    let _normal = thread_mode(Mode::Normal);
    let state = crate::state::tests::test_app_state().await;
    let text = crate::recovery_events::observe_fc_message(&state, "PD approx altitude: 812", 0).unwrap();
    assert!(text.starts_with("Drogue parachute deployed"), "{text}");
}

#[tokio::test]
async fn mission_phase_marks_in_flight_simulated() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let msg = crate::mission_phase::refresh(&state);
    assert!(!msg.simulated);
    assert!(serde_json::to_value(&msg).unwrap().get("simulated").is_none(), "omitted when false");
    observe(&state, report(FcMode::Sim));
    let msg = crate::mission_phase::refresh(&state);
    assert!(msg.simulated);
    assert_eq!(serde_json::to_value(&msg).unwrap()["simulated"], true);
    let wire = crate::web::FlightStateWire::new(FlightState::InFlight, &state);
    assert!(wire.simulated);
    assert_eq!(crate::mission_phase::PhaseMode::current(), crate::mission_phase::PhaseMode::Flight, "no test-fire collapse");
}

#[tokio::test]
async fn recording_start_rows_name_the_station_mode() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    let rows = recording_start_rows(&state);
    let kinds: Vec<_> = rows
        .iter()
        .map(|row| match row {
            crate::telemetry_db::DbWrite::Telemetry { data_type, payload_json, sender_id, .. } => {
                (data_type.clone(), payload_json.clone(), sender_id.clone())
            }
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(kinds[0].0, "GS_STATION_MODE");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&kinds[0].1).unwrap(),
        json!({"mode": "integrated_ground_test", "igt_active": true, "mock_wet_dress": false, "hitl_replay": false,
               "provenance": "hardware", "firmware_sim": false})
    );
    assert_eq!(kinds[0].2, "GS");
    assert_eq!(kinds[1].0, "GS_IGT");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&kinds[1].1).unwrap()["fc_mode"], "off");
    let _normal = thread_mode(Mode::Normal);
    let rows = recording_start_rows(&state);
    assert_eq!(rows.len(), 1, "only an IGT recording carries the FC ground-test status");
}

#[tokio::test]
async fn igt_fills_like_test_fire_but_requires_and_shows_the_av_bay() {
    use crate::types::Board;
    assert_eq!(
        crate::loadcell::calibration_filename_for(Mode::IntegratedGroundTest),
        crate::loadcell::calibration_filename_for(Mode::TestFire)
    );
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    assert!(crate::runtime_mode::is_test_fire_or_igt());
    let state = crate::state::tests::test_app_state().await;
    for board in [Board::FlightComputer, Board::RFBoard, Board::PowerBoard, Board::ValveBoard] {
        assert!(state.board_required_for_progression(board), "{board:?} required in IGT");
    }
    let shown: Vec<Board> = state.board_status_snapshot(1).boards.iter().map(|b| b.board).collect();
    assert!(shown.contains(&Board::FlightComputer) && shown.contains(&Board::RFBoard));
    assert_eq!(crate::safety_catalog::build_mode(), "igt");
    // Board recovery and abort cover the AV bay, as in flight.
    assert_eq!(crate::board_recovery::scope::mode_scope(Mode::IntegratedGroundTest), 0x7f);
    assert_eq!(crate::board_abort::required_unknown_mask(&state, 0x7f), 0x7f);
    assert!(crate::fc_estimator::fc_expected_for_test(&state), "the estimator is expected in IGT");
    let _tf = thread_mode(Mode::TestFire);
    assert!(!state.board_required_for_progression(Board::FlightComputer));
    assert!(!crate::fc_estimator::fc_expected_for_test(&state));
}

// ---------------------------------------------------------------------------
// Round 2: image identification and the Launch latch, settings, HITL replay,
// FC SD retry (docs/integrated-ground-test.md 7.1..7.5).

/// A ground-test image's BOARD_HEALTH (bit 7 set, mode Off).
fn ground_test_health() -> [u32; 24] {
    health(bits(0, false, false, false, false, 0))
}

/// A flight image's BOARD_HEALTH: the tag with bits 4..15 clear.
fn flight_health() -> [u32; 24] {
    health(0)
}

fn launch_control(state: &AppState) -> crate::sequences::ActionControl {
    let mut policy = crate::sequences::default_action_policy();
    for control in &mut policy.controls {
        control.enabled = true;
    }
    decorate_policy(state, &mut policy);
    policy.controls.into_iter().find(|c| c.cmd == "Launch").unwrap()
}

#[test]
fn the_image_is_word_14_bit_7_and_unknown_before_a_report() {
    assert!(decode(&ground_test_health()).unwrap().supported);
    assert!(!decode(&flight_health()).unwrap().supported);
    let tracker = Tracker::default();
    assert_eq!(tracker.fc_image(), FcImage::Unknown);
    assert_eq!(serde_json::to_value(tracker.status()).unwrap()["fc_image"], "unknown");
    tracker.observe(decode(&ground_test_health()).unwrap(), Instant::now(), 1);
    assert_eq!(tracker.fc_image(), FcImage::GroundTest);
    tracker.observe(decode(&flight_health()).unwrap(), Instant::now(), 2);
    assert_eq!(tracker.fc_image(), FcImage::Flight);
    assert_eq!(serde_json::to_value(tracker.status()).unwrap()["fc_image"], "flight");
}

#[tokio::test]
async fn a_ground_test_image_latches_launch_in_normal_and_test_fire_until_a_flight_image() {
    for mode in [Mode::Normal, Mode::TestFire] {
        let _mode = thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        assert!(launch_refusal(&state).is_none(), "{mode:?}");
        // The real ingest path: test fire hides the AV bay, but the FC's
        // BOARD_HEALTH still identifies the image.
        observe_health(&state, &ground_test_health());
        assert!(state.igt.latched(), "{mode:?}");
        assert_eq!(launch_refusal(&state).as_deref(), Some(LAUNCH_LATCH_REASON));
        assert_eq!(
            launch_command_refusal(&state, &TelemetryCommand::GroundStationLaunch).as_deref(),
            Some(LAUNCH_LATCH_REASON)
        );
        let launch = launch_control(&state);
        assert!(!launch.enabled && launch.disabled_reason.as_deref() == Some(LAUNCH_LATCH_REASON));
        let status = serde_json::to_value(state.igt.status()).unwrap();
        assert_eq!(status["launch_latched"], true);
        assert_eq!(status["launch_latch_reason"], LAUNCH_LATCH_REASON);
        assert_eq!(status["image_notice"], json!({"level": "warning", "text": LAUNCH_LATCH_REASON}));
        assert_eq!(status["fc_image"], "ground_test");
        assert!(!state.is_command_allowed(&TelemetryCommand::Launch));
        // The plain command path refuses Launch with the reason.
        let (code, message) = crate::web::dispatch_authorized_command(
            state.clone(),
            principal(true, &[]),
            TelemetryCommand::Launch,
            0,
            None,
            crate::board_abort::capture_launch_revision_named(&state, "Launch"),
        )
        .await;
        assert_ne!(code, StatusCode::OK, "{message}");
        assert!(message.contains(LAUNCH_LATCH_REASON), "{mode:?}: {message}");
        // The FC goes silent: the latch holds.
        {
            let mut inner = state.igt.inner.lock().unwrap();
            inner.at = Some(Instant::now() - Duration::from_secs(600));
            inner.sd = None;
        }
        tick(&state);
        assert!(state.igt.fresh_report(Instant::now()).is_none());
        assert_eq!(launch_refusal(&state).as_deref(), Some(LAUNCH_LATCH_REASON), "silence never clears it");
        // A non-FC-tagged report (older firmware or another board) does not clear it either.
        let mut untagged = flight_health();
        untagged[14] &= 0xFFFF;
        observe_health(&state, &untagged);
        assert!(state.igt.latched());
        // Only a fresh flight-image report clears it.
        observe_health(&state, &flight_health());
        assert!(!state.igt.latched());
        assert!(launch_refusal(&state).is_none());
        assert!(launch_control(&state).disabled_reason.is_none());
        let status = state.igt.status();
        assert!(!status.launch_latched && status.image_notice.is_none() && status.launch_latch_reason.is_none());
    }
}

#[tokio::test]
async fn the_latch_is_set_on_entering_a_latching_mode_and_persists() {
    let _mode = thread_mode(Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    load_persisted(&state);
    observe_health(&state, &ground_test_health());
    assert!(!state.igt.latched(), "HITL never latches");
    let _normal = thread_mode(Mode::Normal);
    tick(&state);
    assert!(state.igt.latched(), "the last image is the ground-test one");
    // A station restart reads the latch back.
    state.igt.inner.lock().unwrap().latched = false;
    load_persisted(&state);
    assert!(state.igt.latched());
    observe_health(&state, &flight_health());
    state.igt.inner.lock().unwrap().latched = false;
    load_persisted(&state);
    assert!(!state.igt.latched(), "the cleared latch is saved too");
}

#[tokio::test]
async fn hitl_only_notifies_a_ground_test_image_and_never_latches() {
    for replay in [false, true] {
        let _mode = thread_mode(Mode::Hitl);
        let state = crate::state::tests::test_app_state().await;
        state.igt.inner.lock().unwrap().settings.hitl_replay = replay;
        observe_health(&state, &ground_test_health());
        assert!(!state.igt.latched());
        let status = state.igt.status();
        assert!(!status.launch_latched);
        let notice = status.image_notice.expect("informational notice");
        assert_eq!(notice.level, NoticeLevel::Info);
        assert!(notice.text.contains("ground-test image"));
        assert_eq!(serde_json::to_value(&notice).unwrap()["level"], "info");
        assert!(latch_refusal(&state).is_none());
        assert!(launch_command_refusal(&state, &TelemetryCommand::Launch).is_none_or(|r| !r.contains("dry-run capable")));
        if !replay {
            assert!(launch_refusal(&state).is_none(), "plain HITL allows Launch");
        }
        let notifications = state.notifications_snapshot();
        assert!(notifications.iter().any(|n| n.message.contains("HITL does not block Launch")), "a notification is raised");
    }
}

#[tokio::test]
async fn igt_mode_expects_the_ground_test_image_and_warns_on_a_flight_image() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    observe_health(&state, &ground_test_health());
    let status = state.igt.status();
    assert!(!status.launch_latched && !state.igt.latched());
    assert_eq!(status.image_notice.as_ref().unwrap().level, NoticeLevel::Info);
    assert!(
        state.notifications_snapshot().iter().all(|n| !n.message.contains("ground-test image")),
        "an indicator only"
    );
    observe_health(&state, &flight_health());
    let notice = state.igt.status().image_notice.unwrap();
    assert_eq!((notice.level, notice.text.as_str()), (NoticeLevel::Warning, FLIGHT_IMAGE_WARNING));
    assert!(state.notifications_snapshot().iter().any(|n| n.message == FLIGHT_IMAGE_WARNING));
    assert!(!state.igt.latched(), "IGT never latches");
    observe_health(&state, &ground_test_health());
    assert_eq!(state.igt.status().image_notice.unwrap().level, NoticeLevel::Info);
    assert!(
        state.notifications_snapshot().iter().all(|n| n.message != FLIGHT_IMAGE_WARNING),
        "the warning clears when a ground-test image appears"
    );
}

#[tokio::test]
async fn hitl_replay_warns_on_a_flight_image() {
    let _mode = thread_mode(Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    observe_health(&state, &flight_health());
    assert!(state.igt.status().image_notice.is_none(), "plain HITL: a flight image is normal");
    state.igt.inner.lock().unwrap().settings.hitl_replay = true;
    tick(&state);
    let notice = state.igt.status().image_notice.unwrap();
    assert_eq!((notice.level, notice.text.as_str()), (NoticeLevel::Warning, FLIGHT_IMAGE_WARNING));
    assert!(state.notifications_snapshot().iter().any(|n| n.message == FLIGHT_IMAGE_WARNING));
    observe_health(&state, &ground_test_health());
    assert_eq!(state.igt.status().image_notice.unwrap().level, NoticeLevel::Info);
    assert!(state.notifications_snapshot().iter().all(|n| n.message != FLIGHT_IMAGE_WARNING));
}

#[tokio::test]
async fn mock_wet_dress_switches_the_igt_fill_to_the_flight_steps() {
    let _mode = thread_mode(Mode::IntegratedGroundTest);
    let state = crate::state::tests::test_app_state().await;
    assert!(test_fire_fill(&state));
    state.igt.inner.lock().unwrap().settings.mock_wet_dress = true;
    assert!(!test_fire_fill(&state), "mock wet dress runs the flight fill steps");
    // The QD and the rest of the stand handling stay as in the IGT.
    assert!(crate::runtime_mode::is_test_fire_or_igt());
    for (mode, expected) in [(Mode::TestFire, true), (Mode::Normal, false), (Mode::Hitl, false), (Mode::Testing, false)] {
        let _m = thread_mode(mode);
        assert_eq!(test_fire_fill(&state), expected, "{mode:?} ignores mock wet dress");
    }
}

#[tokio::test]
async fn settings_are_admin_only_partial_persisted_and_refused_unless_idle() {
    let _mode = thread_mode(Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    load_persisted(&state);
    assert_eq!(state.igt.settings(), Settings::default());
    let operator = principal(false, &["IgtEnter"]);
    assert_eq!(settings::update(&state, &operator, br#"{"hitl_replay":true}"#).unwrap_err().0, StatusCode::FORBIDDEN);
    let admin = principal(true, &[]);
    assert_eq!(settings::update(&state, &admin, br#"{"bogus":true}"#).unwrap_err().0, StatusCode::BAD_REQUEST);
    let saved = settings::update(&state, &admin, br#"{"hitl_replay":true}"#).unwrap();
    assert_eq!(saved, Settings { mock_wet_dress: false, hitl_replay: true });
    let saved = settings::update(&state, &admin, br#"{"mock_wet_dress":true}"#).unwrap();
    assert_eq!(saved, Settings { mock_wet_dress: true, hitl_replay: true }, "partial PUT keeps the other field");
    let status = state.igt.status();
    assert!(status.hitl_replay && status.mock_wet_dress && status.igt_active);
    // Persisted: a restart reads them back.
    state.igt.inner.lock().unwrap().settings = Settings::default();
    load_persisted(&state);
    assert_eq!(state.igt.settings(), Settings { mock_wet_dress: true, hitl_replay: true });
    // Refused unless idle.
    state.set_local_flight_state(FlightState::Armed);
    let (code, error) = settings::update(&state, &admin, br#"{"hitl_replay":false}"#).unwrap_err();
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(error.contains("Armed"), "{error}");
    state.set_local_flight_state(FlightState::Idle);
    state.set_launch_indicator_latched(true);
    assert_eq!(settings::update(&state, &admin, br#"{"hitl_replay":false}"#).unwrap_err().0, StatusCode::CONFLICT);
    state.set_launch_indicator_latched(false);
    assert!(!settings::update(&state, &admin, br#"{"hitl_replay":false}"#).unwrap().hitl_replay);
    assert!(!state.igt.status().igt_active);
}

#[tokio::test]
async fn hitl_replay_routes_launch_to_the_fc_and_holds_phase_steps_during_sim() {
    let _mode = thread_mode(Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    // Ground test controls need the replay.
    assert!(command_gate(&state, &TelemetryCommand::IgtDryRun).unwrap().unwrap_err().contains("HITL replay"));
    assert!(launch_command_refusal(&state, &TelemetryCommand::GroundStationLaunch).is_none());
    state.igt.inner.lock().unwrap().settings.hitl_replay = true;
    assert!(igt_active(&state));
    assert_eq!(command_gate(&state, &TelemetryCommand::IgtDryRun), Some(Ok(())));
    // Launch takes the FC path (FC Launch and VB Sequence) and needs IGT Ready.
    assert!(crate::telemetry_task::launch_goes_to_flight_computer());
    assert!(launch_refusal(&state).unwrap().contains("IGT Ready"));
    observe(&state, report(FcMode::Ready));
    assert!(launch_refusal(&state).is_none());
    // GroundStationLaunch (HITL's VB-only path) is refused while the replay is on.
    let refusal = launch_command_refusal(&state, &TelemetryCommand::GroundStationLaunch).unwrap();
    assert!(refusal.contains("HITL replay"), "{refusal}");
    assert!(!state.is_command_allowed(&TelemetryCommand::GroundStationLaunch));
    // Advance and Rewind are refused while the FC reports Sim.
    assert!(phase_step_refusal(&state).is_none());
    observe(&state, report(FcMode::Sim));
    assert!(state.igt.simulated() && crate::mission_phase::flight_simulated(&state));
    let reason = phase_step_refusal(&state).unwrap();
    for cmd in [TelemetryCommand::AdvanceFlightState, TelemetryCommand::RewindFlightState] {
        assert!(!state.is_command_allowed(&cmd));
    }
    assert!(crate::operator_phase::refusal_reason(&state, true).contains("replay"));
    let mut policy = crate::sequences::default_action_policy();
    for control in &mut policy.controls {
        control.enabled = true;
    }
    decorate_policy(&state, &mut policy);
    for cmd in ["AdvanceFlightState", "RewindFlightState", "GroundStationLaunch"] {
        let control = policy.controls.iter().find(|c| c.cmd == cmd).unwrap();
        assert!(!control.enabled, "{cmd}");
    }
    assert_eq!(
        policy.controls.iter().find(|c| c.cmd == "AdvanceFlightState").unwrap().disabled_reason.as_deref(),
        Some(reason.as_str())
    );
    observe(&state, report(FcMode::Done));
    assert!(phase_step_refusal(&state).is_none(), "they work again at Done");
    // The recording carries the replay's provenance.
    let rows = recording_start_rows(&state);
    assert_eq!(rows.len(), 2);
    // Fill semantics stay HITL's own.
    assert!(!test_fire_fill(&state));
}

#[tokio::test]
async fn fc_sd_service_bit_from_board_health() {
    use crate::igt::fc_sd;
    let state = crate::state::tests::test_app_state().await;
    assert_eq!(fc_sd::service(&state), None, "no report yet");
    let mut words = [0u32; 24];
    assert_eq!(fc_sd::service_from_health(&words), Some(false));
    words[6] = (1 << 29) | (1 << 30) | 0x1F00_0000;
    assert_eq!(fc_sd::service_from_health(&words), Some(true));
    assert_eq!(fc_sd::bits_from_health(&words), Some(fc_sd::NOT_READY), "bit 29 is not an SD state bit");
    assert_eq!(crate::fc_command_gate::bits_from_word6(words[6]), 0x1F, "nor a gate bit");
    assert_eq!(fc_sd::service_from_health(&words[..7]), None);
    fc_sd::observe_service_for_test(&state, true, Instant::now());
    assert_eq!(fc_sd::service(&state), Some(true));
    fc_sd::observe_service_for_test(&state, false, Instant::now());
    assert_eq!(fc_sd::service(&state), Some(false));
    fc_sd::observe_service_for_test(&state, true, Instant::now() - crate::igt::REPORT_FRESH - Duration::from_secs(1));
    assert_eq!(fc_sd::service(&state), None, "a stale report says nothing");
}

#[tokio::test]
async fn fc_sd_alert_and_retry_gate() {
    use crate::igt::fc_sd;
    let _mode = thread_mode(Mode::Normal);
    let state = crate::state::tests::test_app_state().await;
    let now = Instant::now();
    let retry = TelemetryCommand::FcSdRetry;
    assert_eq!(crate::sequences::command_name(&retry), "FcSdRetry");
    assert_eq!(crate::rocket_commands::FlightComputerCommands::FcSdRetry as u8, 33);
    assert_eq!(crate::sequences::control_section("FcSdRetry"), Some(crate::sequences::ControlSection::FlightComputer));
    assert!(fc_sd::retry_gate(&state, now).unwrap_err().contains("No recent FC BOARD_HEALTH"), "stale health");
    assert!(state.igt.status().sd_ready.is_none());
    // Bit 30: the alert, until it clears.
    fc_sd::observe_for_test(&state, fc_sd::NOT_READY, now);
    assert!(state.notifications_snapshot().iter().any(|n| n.message == fc_sd::ALERT));
    assert_eq!(state.igt.status().sd_ready, Some(false));
    assert_eq!(fc_sd::retry_gate(&state, now), Ok(()));
    assert!(state.is_command_allowed(&retry));
    // Bit 31: a retry is pending.
    fc_sd::observe_for_test(&state, fc_sd::NOT_READY | fc_sd::RETRY_PENDING, now);
    assert!(state.igt.status().sd_retry_pending);
    assert!(fc_sd::retry_gate(&state, now).unwrap_err().contains("already pending"));
    assert!(!state.is_command_allowed(&retry));
    // Five seconds between requests.
    fc_sd::observe_for_test(&state, fc_sd::NOT_READY, now);
    fc_sd::note_retry_for_test(&state, now);
    assert!(fc_sd::retry_gate(&state, now + Duration::from_secs(4)).unwrap_err().contains("five seconds"));
    assert_eq!(fc_sd::retry_gate(&state, now + Duration::from_secs(5)), Ok(()));
    // The bits age out with the report.
    assert!(fc_sd::retry_gate(&state, now + Duration::from_secs(13)).is_err());
    fc_sd::observe_for_test(&state, 0, now);
    assert!(state.notifications_snapshot().iter().all(|n| n.message != fc_sd::ALERT), "cleared with the bit");
    assert_eq!(state.igt.status().sd_ready, Some(true));
    // The command gate bits (24..28) and watchdog mask do not read as SD bits.
    let mut words = [0u32; 24];
    words[6] = (0x1F << 24) | 0x00FF_FFFF;
    assert_eq!(fc_sd::bits_from_health(&words), Some(0));
    assert_eq!(crate::fc_command_gate::bits_from_word6(3 << 30), 0, "the FC command gate ignores bits 30..31");
    // Absent in test fire; offered with the FC in every other mode.
    let _tf = thread_mode(Mode::TestFire);
    assert!(fc_sd::retry_gate(&state, now).is_err());
    assert!(serde_json::from_value::<TelemetryCommand>(json!("FcSdRetry")).is_err());
    for mode in [Mode::Normal, Mode::Hitl, Mode::Testing, Mode::IntegratedGroundTest] {
        let _m = thread_mode(mode);
        assert!(serde_json::from_value::<TelemetryCommand>(json!("FcSdRetry")).is_ok(), "{mode:?}");
        assert!(crate::sequences::all_command_names().contains(&"FcSdRetry"), "{mode:?}");
        let layout = crate::layout::load_layout().unwrap();
        assert!(layout.actions_tab.actions.iter().any(|a| a.cmd == "FcSdRetry"), "{mode:?}");
    }
}

#[tokio::test]
async fn igt_status_reaches_clients_in_every_mode_when_it_changes() {
    let _mode = thread_mode(Mode::Normal);
    let state = crate::state::tests::test_app_state().await;
    let mut rx = state.igt.subscribe();
    rx.borrow_and_update();
    observe_health(&state, &ground_test_health());
    assert!(rx.has_changed().unwrap());
    let status = rx.borrow_and_update().clone();
    assert!(status.launch_latched && status.fc_image == FcImage::GroundTest && !status.igt_active);
    assert_eq!(status.sd_ready, Some(true));
}

#[tokio::test]
async fn hitl_offers_the_ground_test_controls_refused_without_the_replay() {
    let _mode = thread_mode(Mode::Hitl);
    let state = crate::state::tests::test_app_state().await;
    for name in IGT_COMMANDS_FOR_TEST {
        assert!(serde_json::from_value::<TelemetryCommand>(json!(name)).is_ok(), "{name}");
        assert!(crate::sequences::all_command_names().contains(&name), "{name}");
    }
    let layout = crate::layout::load_layout().unwrap();
    let cmds: Vec<_> = layout.actions_tab.actions.iter().map(|a| a.cmd.as_str()).collect();
    assert_eq!(&cmds[..4], &IGT_COMMANDS_FOR_TEST);
    assert!(!state.is_command_allowed(&TelemetryCommand::IgtExit));
    let body = br#"{"action":"enter"}"#;
    assert_eq!(prepare(&state, &principal(true, &[]), None, body).unwrap_err().0, StatusCode::CONFLICT);
}
