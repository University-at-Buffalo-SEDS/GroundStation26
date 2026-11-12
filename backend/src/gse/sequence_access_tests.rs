// Admin "launch sequence only" (crate::sequence_access), across every station mode.
use super::*;
use crate::sequence_access::{DISABLED_REASON, disabled_command, launch_only, refusal, set};
use crate::station_mode::Mode;

const MODES: [Mode; 4] = [Mode::Normal, Mode::Hitl, Mode::TestFire, Mode::Testing];

fn enable(state: &AppState, on: bool) {
    set(state, on, Some("administrator".into())).unwrap();
}

/// Sequence commands and whether the setting disables them in `mode`.
fn sequence_commands(mode: Mode) -> Vec<(TelemetryCommand, bool)> {
    let hitl = mode == Mode::Hitl;
    vec![
        (TelemetryCommand::ValveSelfTest, hitl),
        (TelemetryCommand::NitrogenTest, hitl),
        (TelemetryCommand::StartFill, hitl),
        (TelemetryCommand::Turbocharge, true),
        (TelemetryCommand::ContinueFillSequence, true),
    ]
}

/// Commands the setting must never change.
fn unaffected_commands() -> Vec<TelemetryCommand> {
    vec![
        TelemetryCommand::Launch,
        TelemetryCommand::GroundStationLaunch,
        TelemetryCommand::IgniterSequence,
        TelemetryCommand::Abort,
        TelemetryCommand::ResetFlightState,
        TelemetryCommand::ResetLaunchLatch,
        TelemetryCommand::DeployParachute,
        TelemetryCommand::ExpandParachute,
        TelemetryCommand::Dump,
        TelemetryCommand::NormallyOpen,
        TelemetryCommand::Pilot,
        TelemetryCommand::Igniter,
        TelemetryCommand::RetractPlumbing,
        TelemetryCommand::Nitrogen,
        TelemetryCommand::Nitrous,
        TelemetryCommand::NitrogenClose,
        TelemetryCommand::NitrousClose,
        TelemetryCommand::PauseFill,
        TelemetryCommand::CancelFill,
        TelemetryCommand::Postinit,
        TelemetryCommand::StartWritingNow,
        TelemetryCommand::ToggleAvBayUnderglow,
        TelemetryCommand::AdvanceFlightState,
        TelemetryCommand::RewindFlightState,
        TelemetryCommand::ReinitSensors,
    ]
}

fn control(state: &AppState, cmd: &str) -> Option<(bool, Option<String>)> {
    state
        .action_policy_snapshot()
        .controls
        .into_iter()
        .find(|c| c.cmd == cmd)
        .map(|c| (c.enabled, c.disabled_reason))
}

/// Admission and presentation of the commands the setting must not touch.
fn unaffected_view(state: &AppState) -> Vec<(String, bool, Option<(bool, Option<String>)>)> {
    unaffected_commands()
        .iter()
        .map(|cmd| {
            let name = crate::sequences::command_name(cmd);
            (name.to_string(), state.is_command_allowed(cmd), control(state, name))
        })
        .collect()
}

#[tokio::test]
async fn sequences_are_refused_with_the_inline_reason_in_every_mode_and_restored_when_off() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        *state.state.lock().unwrap() = FlightState::Idle;
        // Publish a policy through the normal path (it adds the GSE buttons).
        state.set_action_policy(crate::sequences::default_action_policy());
        let baseline: Vec<bool> = sequence_commands(mode)
            .iter()
            .map(|(cmd, _)| state.is_command_allowed(cmd))
            .collect();
        enable(&state, true);
        assert!(launch_only(&state));
        for (cmd, disabled) in sequence_commands(mode) {
            let name = crate::sequences::command_name(&cmd);
            assert_eq!(refusal(&state, &cmd).is_some(), disabled, "{mode:?} {name}");
            if disabled {
                assert!(!state.is_command_allowed(&cmd), "{mode:?} {name}");
                assert_eq!(
                    command_refusal_reason(&state, &cmd).as_deref(),
                    Some(DISABLED_REASON),
                    "{mode:?} {name}"
                );
                if let Some((enabled, reason)) = control(&state, name) {
                    assert!(!enabled, "{mode:?} {name}");
                    assert_eq!(reason.as_deref(), Some(DISABLED_REASON), "{mode:?} {name}");
                }
            }
        }
        // The Turbocharge button is always published; it must carry the reason.
        assert_eq!(
            control(&state, "Turbocharge").unwrap().1.as_deref(),
            Some(DISABLED_REASON),
            "{mode:?}"
        );
        // Auto Cold Flow: its own status names the reason and refuses a start.
        let cold = cold_flow::status(&state);
        assert!(!cold.start_available, "{mode:?}");
        assert_eq!(cold.reason, DISABLED_REASON, "{mode:?}");
        assert!(disabled_command(&state, "AutoColdFlow"));

        enable(&state, false);
        for ((cmd, _), before) in sequence_commands(mode).iter().zip(&baseline) {
            assert!(refusal(&state, cmd).is_none());
            assert_eq!(state.is_command_allowed(cmd), *before, "{mode:?} {cmd:?}");
        }
        assert_ne!(cold_flow::status(&state).reason, DISABLED_REASON, "{mode:?}");
        assert_ne!(
            control(&state, "Turbocharge").unwrap().1.as_deref(),
            Some(DISABLED_REASON)
        );
    }
}

#[tokio::test]
async fn launch_abort_reset_recovery_and_manual_commands_are_unchanged_in_every_mode() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        for armed in [false, true] {
            let (state, path, _peers) = if armed {
                let (state, path, peers) = launch_rig(mode).await;
                (state, path, Some(peers))
            } else {
                let state = crate::state::tests::test_app_state().await;
                *state.state.lock().unwrap() = FlightState::Idle;
                (state, None, None)
            };
            if armed {
                assert!(state.is_command_allowed(&TelemetryCommand::Launch), "{mode:?}");
            }
            let off = unaffected_view(&state);
            enable(&state, true);
            let on = unaffected_view(&state);
            assert_eq!(off, on, "{mode:?} armed={armed}");
            for cmd in unaffected_commands() {
                assert!(refusal(&state, &cmd).is_none());
            }
            assert!(state.is_command_allowed(&TelemetryCommand::Abort), "{mode:?}");
            if let Some(path) = path {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// The armed Launch fixture the existing Launch suites use, with warm
/// ignition-pair peers, so Launch admission depends only on the gate under test.
async fn launch_rig(
    mode: Mode,
) -> (Arc<AppState>, Option<std::path::PathBuf>, crate::ignition_pair::fixture::Peers) {
    use sedsnet::router::{Router, RouterConfig};
    let state = crate::state::tests::test_app_state().await;
    if mode == Mode::TestFire {
        let router = Arc::new(Router::new(
            RouterConfig::new([crate::ignition_pair::fixture::handler(&state, |_| Ok(()))])
                .with_sender("GS"),
        ));
        let path = armed_fixture_with_real_pair_for_test(&state, &router);
        let peers = crate::ignition_pair::fixture::Peers::warm_pair_only(&state, router.clone()).await;
        return (state, Some(path), peers);
    }
    let router = Arc::new(Router::new(RouterConfig::new([
        crate::ignition_pair::fixture::handler(&state, |_| Ok(())),
    ])));
    let valve_side = router.add_side_packed_with_options(
        "umbilical_comms",
        |_| Ok(()),
        sedsnet::router::RouterSideOptions {
            reliable_enabled: true,
            link_local_enabled: true,
            ..Default::default()
        },
    );
    let discovery = sedsnet::discovery::build_discovery_announce(
        crate::types::Board::ValveBoard.sender_id(),
        123,
        &[crate::telemetry_schema::endpoint("VALVE_BOARD")],
    )
    .unwrap();
    router
        .rx_packed_from_side(&sedsnet::wire_format::pack_packet(&discovery), valve_side)
        .unwrap();
    router.process_all_queues_with_timeout(0).unwrap();
    let path = if mode == Mode::Hitl {
        state.set_local_flight_state(FlightState::Armed);
        None
    } else {
        Some(armed_fixture_for_test(&state, &router))
    };
    if state.topology_router.get().is_none() {
        state.topology_router.set(router.clone()).unwrap();
    }
    let peers = crate::ignition_pair::fixture::Peers::warm(&state, router.clone()).await;
    (state, path, peers)
}

#[tokio::test]
async fn qd_and_retraction_launch_gates_are_untouched_and_abort_works_in_every_mode() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let (state, path, _peers) = launch_rig(mode).await;
        enable(&state, true);
        let launch = |state: &AppState| {
            (
                state.is_command_allowed(&TelemetryCommand::Launch),
                state.is_command_allowed(&TelemetryCommand::GroundStationLaunch),
            )
        };
        let compare = |state: &AppState, label: &str| {
            let on = launch(state);
            enable(state, false);
            let off = launch(state);
            enable(state, true);
            assert_eq!(on, off, "{mode:?} {label}: the setting changed launch admission");
            assert!(state.is_command_allowed(&TelemetryCommand::Abort), "{mode:?} {label}");
            on
        };
        // The rig is otherwise launch-ready: every remaining refusal below
        // comes from the QD / retraction gate under test.
        assert_eq!(compare(&state, "ready"), (true, true), "{mode:?}");
        let revision = || crate::board_abort::capture_launch_revision_named(&state, "Launch");
        if mode != Mode::Hitl {
            // QD wait: not started (no retraction yet) refuses Launch in the
            // modes whose launch gate requires it. TestFire's launch gate has
            // never used this wait (it uses the plumbing retraction gate).
            {
                let mut rt = state.gse.lock().unwrap();
                rt.operational.qd = QdWait::default();
            }
            let not_started = compare(&state, "QD not started");
            if matches!(mode, Mode::Normal | Mode::Testing) {
                assert_eq!(not_started, (false, false), "{mode:?}");
                let (enabled, reason) = control(&state, "Launch").unwrap();
                assert!(!enabled && reason.is_some_and(|r| !r.is_empty()), "{mode:?}");
            }
            // QD dispatched, awaiting its acknowledgement (moving).
            {
                let mut rt = state.gse.lock().unwrap();
                let now = rt.now();
                rt.operational.qd.dispatched(now);
            }
            let moving = compare(&state, "QD awaiting acknowledgement");
            if matches!(mode, Mode::Normal | Mode::Testing) {
                // A press while QD moves is held, never sent, until QD is
                // acknowledged and settled.
                assert_eq!(moving, (true, true));
                assert!(super::operational_policy::handle_operational_command(
                    &state,
                    &TelemetryCommand::Launch,
                    None,
                    revision(),
                ));
                assert!(state.gse.lock().unwrap().operational.qd.queued(), "{mode:?}");
                assert!(state.is_command_allowed(&TelemetryCommand::Abort));
            }
            // QD acknowledged and settled.
            {
                let mut rt = state.gse.lock().unwrap();
                rt.operational.qd = QdWait::default();
                rt.operational.qd.dispatched(0);
                rt.operational.qd.confirm(1);
                assert_eq!(rt.operational.qd.status(rt.now()), QdStatus::Ready);
            }
            assert_eq!(compare(&state, "QD complete"), (true, true), "{mode:?}");
            // Complete: Launch passes the QD stage to the actual send path.
            assert!(!super::operational_policy::handle_operational_command(
                &state,
                &TelemetryCommand::Launch,
                None,
                revision(),
            ));
        }
        // Fill-plumbing retraction in motion (owner pending) refuses Launch
        // in every mode, with its inline reason.
        state.plumbing_drive.begin_settings();
        let gate = crate::actuator_plumbing::retraction_gate(&state);
        assert!(!gate.allowed && gate.phase == "pending", "{mode:?}");
        assert_eq!(compare(&state, "retraction pending"), (false, false), "{mode:?}");
        let (enabled, reason) = control(&state, "Launch").unwrap();
        assert!(!enabled && reason.is_some_and(|r| !r.is_empty()), "{mode:?}");
        assert!(launch_only(&state));
        if let Some(path) = path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[tokio::test]
async fn hitl_engine_sequences_and_automatic_fill_start_are_refused_while_enabled() {
    let _mode = crate::runtime_mode::test_support::thread_mode(Mode::Hitl);
    let (state, sent, peer) = tests::button_test_state().await;
    assert_eq!(command_allowed(&state, &TelemetryCommand::NitrogenTest), Some(true));
    enable(&state, true);
    assert!(!state.is_command_allowed(&TelemetryCommand::NitrogenTest));
    // Every path into the engine (operator, recorded fill, automatic zero
    // then fill, resume) passes handle_command_inner, which refuses.
    for cmd in [
        TelemetryCommand::NitrogenTest,
        TelemetryCommand::ValveSelfTest,
        TelemetryCommand::StartFill,
        TelemetryCommand::Turbocharge,
    ] {
        assert!(handle_command(&state, &cmd));
        pump_test_link_for(&state, &peer);
        let rt = state.gse.lock().unwrap();
        assert_eq!(rt.engine.status.phase, Phase::Idle, "{cmd:?}");
        assert!(rt.turbocharge.is_none());
    }
    assert!(sent.lock().unwrap().is_empty(), "no valve command may leave");
    // Off again: the same request starts the nitrogen test.
    enable(&state, false);
    assert!(handle_command(&state, &TelemetryCommand::NitrogenTest));
    assert!(state.gse.lock().unwrap().engine.active());
    // Turning it on while that sequence runs is refused, never a silent stop.
    let error = set(&state, true, None).unwrap_err();
    assert_eq!(error.0, axum::http::StatusCode::CONFLICT);
    assert!(error.1.contains("nitrogen test"), "{}", error.1);
    assert!(!launch_only(&state));
    assert!(state.gse.lock().unwrap().engine.active());
    assert!(
        crate::sequence_access::status(&state)
            .enable_blocked_reason
            .is_some_and(|reason| reason.contains("nitrogen test"))
    );
}

fn pump_test_link_for(state: &AppState, peer: &sedsnet::router::Router) {
    tests::pump_test_link(state, peer);
}

#[tokio::test]
async fn kept_sequences_do_not_block_the_toggle_outside_hitl_but_do_in_hitl() {
    for mode in MODES {
        let _mode = crate::runtime_mode::test_support::thread_mode(mode);
        let state = crate::state::tests::test_app_state().await;
        state.gse.lock().unwrap().engine.status.phase = Phase::Raising;
        let result = set(&state, true, None);
        if mode == Mode::Hitl {
            assert_eq!(result.unwrap_err().0, axum::http::StatusCode::CONFLICT);
            assert!(!launch_only(&state));
        } else {
            result.unwrap();
            assert!(launch_only(&state));
        }
    }
}

#[tokio::test]
async fn setting_persists_across_restart_and_defaults_off() {
    let first = crate::state::tests::test_app_state().await;
    crate::sequence_access::restore(&first);
    assert!(!launch_only(&first), "default is off");
    enable(&first, true);
    let reopen = |placeholder: String| async move {
        let mut state = crate::state::tests::test_app_state().await;
        Arc::get_mut(&mut state).unwrap().placeholder_db_path = placeholder;
        crate::sequence_access::restore(&state);
        state
    };
    let second = reopen(first.placeholder_db_path.clone()).await;
    assert!(launch_only(&second));
    enable(&second, false);
    let third = reopen(first.placeholder_db_path.clone()).await;
    assert!(!launch_only(&third));
}

#[tokio::test]
async fn admin_route_requires_an_administrator() {
    let root = crate::recording_export::tests::TestDirectory::new();
    let state = crate::recording_export::tests::export_state(&root.0, true).await;
    std::fs::write(root.0.join("auth.json"), serde_json::json!({"users":[
        {"username":"administrator","roles":["admin"],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true}},
        {"username":"operator","roles":[],"password":{"salt_b64":"","hash_b64":""},"permissions":{"view_data":true,"send_commands":true}}
    ]}).to_string()).unwrap();
    for (token, name) in [("admin-test", "administrator"), ("operator-test", "operator")] {
        sqlx::query("INSERT INTO auth_sessions(token,username,session_type,can_view_data,can_send_commands,created_at_ms,expires_at_ms) VALUES(?,?,'session',1,1,0,9999999999999)")
            .bind(token).bind(name).execute(&state.auth_db).await.unwrap();
    }
    let app = crate::sequence_access::routes().with_state(state.clone());
    let request = |token: &str, body: Option<&str>| {
        let builder = axum::http::Request::builder()
            .uri("/api/admin/sequence-access")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        match body {
            Some(body) => builder.method("POST").body(axum::body::Body::from(body.to_string())),
            None => builder.method("GET").body(axum::body::Body::empty()),
        }
        .unwrap()
    };
    use tower::ServiceExt;
    let refused = app
        .clone()
        .oneshot(request("operator-test", Some(r#"{"launch_sequence_only":true}"#)))
        .await
        .unwrap();
    assert_eq!(refused.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(!launch_only(&state));
    let accepted = app
        .clone()
        .oneshot(request("admin-test", Some(r#"{"launch_sequence_only":true}"#)))
        .await
        .unwrap();
    assert_eq!(accepted.status(), axum::http::StatusCode::OK);
    assert!(launch_only(&state));
    let body = axum::body::to_bytes(
        app.oneshot(request("admin-test", None)).await.unwrap().into_body(),
        65536,
    )
    .await
    .unwrap();
    let status: crate::sequence_access::Status = serde_json::from_slice(&body).unwrap();
    assert!(status.launch_sequence_only);
    assert!(status.disabled_sequences.iter().any(|s| s == "Turbocharge"));
    assert!(status.always_kept.iter().any(|s| s.contains("Abort")));
    assert!(status.always_kept.iter().any(|s| s.contains("QD")));
}
