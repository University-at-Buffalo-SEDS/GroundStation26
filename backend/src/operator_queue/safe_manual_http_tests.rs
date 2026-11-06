use super::*;
use std::sync::{Mutex, atomic::Ordering};
use tower::ServiceExt;

async fn fixture() -> (
    crate::recording_export::tests::TestDirectory,
    Arc<AppState>,
    HeaderMap,
    Arc<Mutex<Vec<sedsnet::packet::Packet>>>,
    mpsc::Receiver<TelemetryCommand>,
) {
    let (root, state, rx, headers) = command_dispatch_tests::preauth_fixture().await;
    let observed = Arc::new(Mutex::new(Vec::new()));
    let handlers = ["ACTUATOR_BOARD", "VALVE_BOARD"].map(|owner| {
        let sink = observed.clone();
        sedsnet::router::EndpointHandler::new_packet_handler(
            crate::telemetry_schema::endpoint(owner),
            move |packet| {
                sink.lock().unwrap().push(packet.clone());
                Ok(())
            },
        )
    });
    let router = Arc::new(sedsnet::router::Router::new(
        sedsnet::router::RouterConfig::new(handlers)
            .with_sender("GS")
            .with_memory_config(
                sedsnet::config::RuntimeMemoryConfig::new(8 * 1024 * 1024, 8192, 1024 * 1024, 2.0)
                    .unwrap(),
            )
            .unwrap(),
    ));
    router
        .try_set_local_network_time(sedsnet::timesync::PartialNetworkTime::from_unix_ms(
            1_793_563_200_000,
        ))
        .unwrap();
    state.topology_router.set(router).unwrap();
    {
        // The normal-build fixture starts with all manual controls disabled.
        // Model an authenticated operator with the hardware key enabled; do
        // not confuse this independent policy with recovery admission.
        let mut policy = state.action_policy.lock().unwrap();
        policy.key_enabled = true;
        policy.software_buttons_enabled = true;
        for control in &mut policy.controls {
            control.enabled = true;
        }
    }
    state
        .operator_admission_barrier
        .store(77, Ordering::Release);
    (root, state, headers, observed, rx)
}
async fn http(state: Arc<AppState>, headers: HeaderMap, name: &str) -> Response {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/api/command")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(name).unwrap()))
        .unwrap();
    request.headers_mut().extend(headers);
    Router::new()
        .route("/api/command", post(send_command_entry))
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap()
}
#[tokio::test]
async fn safe_manual_http_relief_crosses_retained_recovery_without_reopening_or_retargeting() {
    // Isolated SDK fixtures leave each packet's actual local delivery visible;
    // the fixture has no second peer to acknowledge reliable transport retries.
    for (name, channel, on, byte) in [
        ("NormallyOpen", 1, false, 1),
        ("Dump", 2, false, 2),
        ("Pilot", 0, true, 3),
        ("Igniter", 7, true, 11),
        ("NitrogenClose", 9, true, 12),
        ("NitrousClose", 10, true, 13),
    ] {
        let (_root, state, headers, observed, _commands) = fixture().await;
        let before = crate::operator_queue::Token::capture(&state, 77);
        state.set_umbilical_valve_state(channel, on);
        let response = http(state.clone(), headers.clone(), name).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{name}");
        assert_eq!(observed.lock().unwrap().last().unwrap().payload(), &[byte]);
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
        assert_eq!(crate::gse::automatic_generation(&state), before.generation);
        assert!(
            crate::gse::with_closed_operator_queue(&state, before, || ()).is_none(),
            "old recovery quiescence must be retired"
        );
        assert!(crate::gse::capture_operator_origin(&state).is_none());
        assert_eq!(state.operator_dispatch_inflight.load(Ordering::Acquire), 0);
    }
}
#[tokio::test]
async fn safe_manual_http_requires_auth_software_enable_and_known_safe_polarity() {
    let (_root, state, headers, observed, _commands) = fixture().await;
    assert_eq!(
        http(state.clone(), HeaderMap::new(), "NitrogenClose")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    state.action_policy.lock().unwrap().software_buttons_enabled = false;
    assert_eq!(
        http(state.clone(), headers.clone(), "NitrogenClose")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    state.action_policy.lock().unwrap().software_buttons_enabled = true;
    for known in [None, Some(true)] {
        if let Some(on) = known {
            state.set_umbilical_valve_state(1, on);
        }
        let _ = http(state.clone(), headers.clone(), "NormallyOpen").await;
        assert!(
            observed.lock().unwrap().is_empty(),
            "unknown/unsafe toggle never uses the relief lane"
        );
    }
    assert!(observed.lock().unwrap().is_empty());
    assert_eq!(
        http(state.clone(), headers, "Abort").await.status(),
        StatusCode::ACCEPTED
    );
    assert!(state.manual_abort_intent.pending_epoch().is_some());
}
#[tokio::test]
async fn safe_manual_http_original_intent_rejects_new_stop_owner_generation_and_auth_delay() {
    for mutation in 0..4 {
        let (_root, state, headers, observed, _commands) = fixture().await;
        state.set_umbilical_valve_state(1, false);
        let (arrived, release) =
            crate::telemetry_task::recording_owner::test_hooks::hold_http_auth(&state);
        let task = tokio::spawn(http(state.clone(), headers, "NormallyOpen"));
        tokio::task::spawn_blocking(move || {
            arrived.recv_timeout(StdDuration::from_secs(2)).unwrap()
        })
        .await
        .unwrap();
        match mutation {
            0 => {
                state.manual_abort_intent.request();
            }
            1 => state
                .operator_admission_barrier
                .store(78, Ordering::Release),
            2 => {
                state.automatic_generation.fetch_add(1, Ordering::AcqRel);
            }
            3 => {
                state.set_umbilical_valve_state(1, true);
            } // still sends sealed OPEN, never toggle CLOSE
            _ => unreachable!(),
        }
        release.send(()).unwrap();
        let response = task.await.unwrap();
        if mutation == 3 {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert_eq!(observed.lock().unwrap().last().unwrap().payload(), &[1]);
        } else {
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert!(observed.lock().unwrap().is_empty());
        }
    }
}
#[tokio::test]
async fn safe_manual_websocket_frame_keeps_abort_lane_and_relief_available() {
    let (_root, state, _headers, observed, _commands) = fixture().await;
    state.set_umbilical_valve_state(2, false);
    handle_ws_control_text_with_source(
        state.clone(),
        Some("preauth-test".into()),
        state.cmd_tx.clone(),
        r#"{"cmd":"Dump"}"#.into(),
        Arc::new(|| true),
    )
    .await;
    tokio::time::timeout(StdDuration::from_secs(2), async {
        loop {
            if !observed.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(observed.lock().unwrap().last().unwrap().payload(), &[2]);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
    handle_ws_control_text_with_source(
        state.clone(),
        Some("preauth-test".into()),
        state.cmd_tx.clone(),
        r#"{"cmd":"Abort"}"#.into(),
        Arc::new(|| true),
    )
    .await;
    assert!(state.manual_abort_intent.pending_epoch().is_some());
}
#[tokio::test]
async fn safe_manual_http_open_origin_cannot_gain_privilege_after_recovery_closes() {
    let (_root, state, headers, observed, _commands) = fixture().await;
    state.operator_admission_barrier.store(0, Ordering::Release);
    let (arrived, release) =
        crate::telemetry_task::recording_owner::test_hooks::hold_http_auth(&state);
    let task = tokio::spawn(http(state.clone(), headers, "NitrogenClose"));
    tokio::task::spawn_blocking(move || arrived.recv_timeout(StdDuration::from_secs(2)).unwrap())
        .await
        .unwrap();
    state
        .operator_admission_barrier
        .store(77, Ordering::Release);
    release.send(()).unwrap();
    assert!(!task.await.unwrap().status().is_success());
    assert!(observed.lock().unwrap().is_empty());
    assert_eq!(state.operator_queue.relief_epoch(), 0);
}
#[tokio::test]
async fn manual_origin_http_fresh_recording_command_works_with_retained_recovery_and_old_queue_does_not()
 {
    let (_root, state, headers, _observed, mut commands) = fixture().await;
    let generation = crate::gse::automatic_generation(&state);
    assert!(crate::gse::claim_operator_dispatch(&state, generation).is_none());
    let old = crate::operator_queue::Token::capture(&state, 77);
    let response = http(state.clone(), headers, "StartWritingNow").await;
    assert_eq!(response.status(), StatusCode::OK);
    let TelemetryCommand::QueuedOperator {
        generation: queued_generation,
        recovery_origin,
        command,
        ..
    } = commands.recv().await.unwrap()
    else {
        panic!("real HTTP must enqueue private operator envelope")
    };
    assert!(matches!(*command, TelemetryCommand::StartWritingNow));
    assert!(recovery_origin.is_some());
    let claim =
        crate::gse::claim_manual_dispatch(&state, queued_generation, recovery_origin).unwrap();
    assert!(
        crate::gse::with_closed_operator_queue(&state, old, || panic!(
            "old coordinator must not drain the new command"
        ))
        .is_none()
    );
    drop(claim);
    assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
    assert!(
        crate::gse::claim_operator_dispatch(&state, generation).is_none(),
        "old ordinary origins never gain the new privilege"
    );
    state.manual_abort_intent.request();
    assert!(
        crate::gse::claim_manual_dispatch(&state, queued_generation, recovery_origin).is_none()
    );
}
#[tokio::test]
async fn manual_origin_http_and_websocket_physical_commands_reach_actual_telemetry_worker() {
    for websocket in [false, true] {
        let (_root, state, headers, observed, commands) = fixture().await;
        state.set_umbilical_valve_state(10, false);
        state.set_umbilical_valve_state(9, false); // fresh manual OPEN, not safe-relief shortcut
        let (_db_tx, db_rx) = mpsc::channel(8);
        let router = state.topology_router.get().unwrap().clone();
        let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            commands,
            db_rx,
            state.shutdown_subscribe(),
        ));
        if websocket {
            handle_ws_control_text_with_source(
                state.clone(),
                Some("preauth-test".into()),
                state.cmd_tx.clone(),
                r#"{"cmd":"Nitrogen"}"#.into(),
                Arc::new(|| true),
            )
            .await;
        } else {
            assert_eq!(
                http(state.clone(), headers, "Nitrogen").await.status(),
                StatusCode::OK
            );
        }
        let delivered = tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                if observed.lock().unwrap().iter().any(|p| {
                    p.data_type() == crate::telemetry_schema::data_type("ACTUATOR_COMMAND")
                        && p.payload() == [9]
                }) {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
        .await;
        state.request_shutdown();
        worker.await.unwrap();
        assert!(
            delivered.is_ok(),
            "fresh authenticated manual open never reached actual SDK path; websocket={websocket}; notifications={:?}; commands={:?}; observed={:?}",
            state.notifications_snapshot(),
            state.last_command_ms.lock().unwrap(),
            observed
                .lock()
                .unwrap()
                .iter()
                .map(|p| (p.data_type(), p.payload().to_vec()))
                .collect::<Vec<_>>()
        );
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
    }
}
#[cfg(feature = "hitl_mode")]
#[tokio::test]
async fn manual_origin_fresh_http_and_websocket_phase_receipt_works_without_resealing_old_intent() {
    for websocket in [false, true] {
        let (_root, state, headers, _observed, commands) = fixture().await;
        if !crate::operator_phase::local_mode(&state) {
            crate::gse::handle_command_with_origins(
                &state,
                &TelemetryCommand::ToggleGroundStationControl,
                None,
                None,
            );
        }
        state
            .launch_history
            .lock()
            .unwrap()
            .inhibit_automatic_actions()
            .unwrap();
        let old_phase = crate::operator_phase::seal(&state, TelemetryCommand::AdvanceFlightState);
        let router = state.topology_router.get().unwrap().clone();
        router
            .enable_managed_variable(crate::telemetry_schema::data_type("FLIGHT_STATE"))
            .unwrap();
        let (_db_tx, db_rx) = mpsc::channel(8);
        let worker = tokio::spawn(crate::telemetry_task::telemetry_task(
            state.clone(),
            router,
            Vec::new(),
            commands,
            db_rx,
            state.shutdown_subscribe(),
        ));
        if websocket {
            handle_ws_control_text_with_source(
                state.clone(),
                Some("preauth-test".into()),
                state.cmd_tx.clone(),
                r#"{"cmd":"AdvanceFlightState"}"#.into(),
                Arc::new(|| true),
            )
            .await;
        } else {
            assert_eq!(
                http(state.clone(), headers, "AdvanceFlightState")
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        let changed = tokio::time::timeout(StdDuration::from_secs(2), async {
            while state.local_flight_state_snapshot() != FlightState::Idle {
                tokio::time::sleep(StdDuration::from_millis(5)).await;
            }
        })
        .await;
        state.request_shutdown();
        worker.await.unwrap();
        assert!(
            changed.is_ok(),
            "phase action failed websocket={websocket}; notes={:?}",
            state.notifications_snapshot()
        );
        let TelemetryCommand::OperatorPhase { intent } = old_phase else {
            panic!("original phase must be sealed")
        };
        assert!(
            !intent.validate(&state),
            "unrelated earlier phase must not acquire the new receipt"
        );
        assert_eq!(state.operator_admission_barrier.load(Ordering::Acquire), 77);
    }
}
#[tokio::test]
async fn manual_origin_fill_exclusion_uses_ordered_actual_valve_sequence_not_retained_launch() {
    let (_root, state, _headers, _observed, _commands) = fixture().await;
    state
        .launch_history
        .lock()
        .unwrap()
        .inhibit_automatic_actions()
        .unwrap();
    assert!(
        !state.operator_queue.fill_launch_active(),
        "retained admission does not prove active sequence"
    );
    tokio::time::sleep(StdDuration::from_millis(10)).await;
    let base = crate::telemetry_task::get_current_timestamp_ms() - 5;
    let packet = |sender: &str, source, on| {
        sedsnet::packet::Packet::new(
            crate::telemetry_schema::data_type("UMBILICAL_STATUS"),
            &[crate::telemetry_schema::endpoint("GROUND_STATION")],
            sender,
            source,
            Arc::from(&[6, on][..]),
        )
        .unwrap()
    };
    crate::telemetry_task::apply_umbilical_status_at_ingress(&state, &packet("AB", base, 1));
    assert!(
        !state.operator_queue.fill_launch_active(),
        "AB cannot own valve sequence status"
    );
    crate::telemetry_task::apply_umbilical_status_at_ingress(&state, &packet("VB", base + 1, 1));
    assert!(state.operator_queue.fill_launch_active());
    assert!(!state.is_command_allowed(&TelemetryCommand::Nitrogen));
    assert!(state.is_command_allowed(&TelemetryCommand::StartWritingNow));
    crate::telemetry_task::apply_umbilical_status_at_ingress(&state, &packet("VB", base, 0));
    assert!(
        state.operator_queue.fill_launch_active(),
        "old inactive status cannot release newer active sequence"
    );
    crate::telemetry_task::apply_umbilical_status_at_ingress(&state, &packet("AB", base + 2, 0));
    assert!(
        state.operator_queue.fill_launch_active(),
        "spoofed inactive owner cannot release sequence"
    );
    crate::telemetry_task::apply_umbilical_status_at_ingress(&state, &packet("VB", base + 2, 0));
    assert!(!state.operator_queue.fill_launch_active());
}
